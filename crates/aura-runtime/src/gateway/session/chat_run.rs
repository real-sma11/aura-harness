//! Reattachable chat-run registry and per-run event channel.
//!
//! Mirrors the automaton-side
//! [`aura_engine::automaton::EventChannel`] model so a chat run's
//! turn execution can be decoupled from any single WebSocket
//! connection. Where the automaton channel replays
//! [`aura_automaton::AutomatonEvent`]s, this one replays
//! [`OutboundMessage`]s (the chat wire protocol).
//!
//! Motivation: today a chat run is one-shot — the first
//! `WS /stream/:run_id` consumes the prepared [`super::Session`] and a
//! WS close cancels the in-flight turn, so a dropped server↔harness
//! socket kills the turn and a reconnect 409s. By giving each run an
//! independent driver task that owns the `Session` and emits into this
//! channel, a dropped WS can be re-established: a reattaching client
//! replays the run's history and then continues live, exactly like the
//! automaton `/stream/:run_id` path.
//!
//! Concurrency model:
//! - [`ChatEventChannel::push`] appends to the replay `history` and
//!   broadcasts the same message while holding the history lock, so a
//!   concurrent [`ChatEventChannel::subscribe`] (which also takes the
//!   lock before snapshotting history + creating its live receiver)
//!   observes each message exactly once — never both in the replayed
//!   history and again on the live receiver, and never dropped between
//!   the two.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::OutboundMessage;

/// Live broadcast ring capacity. A slow attached client that lags past
/// this many buffered messages observes `RecvError::Lagged`; the WS
/// adapter surfaces that as a progress/warning frame rather than
/// silently dropping content.
const CHAT_EVENT_BROADCAST_CAPACITY: usize = 512;

/// Cap on the per-run replay buffer. Mirrors the broadcast ring so an
/// attached client that keeps up and a late attach that relies on
/// replay observe the same visible window. Exceeding the cap drops the
/// oldest entries first (whole-session history with monotonic ordering;
/// see plan Part C decision iv).
const CHAT_EVENT_HISTORY_CAPACITY: usize = CHAT_EVENT_BROADCAST_CAPACITY;

/// How long the driver lets a run sit fully idle — no active turn and
/// no attached client — before reaping it from the registry. Provides
/// the reconnect grace window for a dropped server↔harness socket
/// (plan Part C decision ii: one `run_id` per chat-session lifetime,
/// removed after this window with no active attaches).
pub(crate) const CHAT_RUN_IDLE_RETENTION: Duration = Duration::from_secs(300);

/// Replay-aware broadcast channel for a single chat run.
pub(crate) struct ChatEventChannel {
    /// Replay history, capped at [`CHAT_EVENT_HISTORY_CAPACITY`]
    /// (oldest-first eviction). Cloned on each [`Self::subscribe`].
    history: Mutex<Vec<OutboundMessage>>,
    /// Live broadcast for currently-attached clients. Retained inside
    /// the `Arc<ChatEventChannel>` so the sender outlives the driver's
    /// forwarder task and late subscribers don't see `Closed` before
    /// draining history.
    broadcast: broadcast::Sender<OutboundMessage>,
    /// Set once the run's driver has stopped. Lets a late attach skip
    /// the live-receive loop after replaying history.
    done: AtomicBool,
}

/// Snapshot returned by [`ChatEventChannel::subscribe`]: replay
/// `history` (consume first, in order) plus a `live` receiver (consume
/// next). Produces the same ordering an early subscriber would have
/// seen, with each message delivered exactly once across the two.
pub(crate) struct ChatEventSubscription {
    pub(crate) history: Vec<OutboundMessage>,
    pub(crate) live: broadcast::Receiver<OutboundMessage>,
    pub(crate) already_done: bool,
}

impl ChatEventChannel {
    pub(crate) fn new() -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(CHAT_EVENT_BROADCAST_CAPACITY);
        Arc::new(Self {
            history: Mutex::new(Vec::new()),
            broadcast: broadcast_tx,
            done: AtomicBool::new(false),
        })
    }

    /// Append a message to the replay history and broadcast it to live
    /// subscribers. History append and broadcast happen under the same
    /// lock so [`Self::subscribe`] sees each message exactly once.
    pub(crate) fn push(&self, msg: OutboundMessage) {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history.len() >= CHAT_EVENT_HISTORY_CAPACITY {
            let drop_n = history.len() + 1 - CHAT_EVENT_HISTORY_CAPACITY;
            history.drain(..drop_n);
        }
        history.push(msg.clone());
        // Broadcast while holding the history lock so a concurrent
        // `subscribe` (which takes the same lock before creating its
        // receiver) cannot interleave between the append and the send.
        let _ = self.broadcast.send(msg);
    }

    pub(crate) fn subscribe(&self) -> ChatEventSubscription {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Create the live receiver while holding the history lock; see
        // `push` for the exactly-once ordering argument.
        let live = self.broadcast.subscribe();
        let snapshot = history.clone();
        drop(history);
        ChatEventSubscription {
            history: snapshot,
            live,
            already_done: self.done.load(Ordering::Acquire),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
    }
}

/// Spawn the per-run forwarder that drains the driver's outbound mpsc
/// into the replay history + live broadcast. Returns the sender the
/// driver (and its turn tasks / approval broker) clone to emit
/// outbound messages.
pub(crate) fn spawn_event_forwarder(
    channel: Arc<ChatEventChannel>,
) -> mpsc::Sender<OutboundMessage> {
    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(1024);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            channel.push(msg);
        }
    });
    tx
}

/// Registry entry for a live chat run. Shared between the registry
/// (`RouterState::chat_runs`), the driver task, and every attached WS
/// adapter.
pub(crate) struct ChatRunHandle {
    /// Inbound command channel feeding the driver. WS adapters clone
    /// this to forward parsed [`crate::protocol::InboundMessage`]s.
    pub(crate) commands: mpsc::Sender<crate::protocol::InboundMessage>,
    /// Replay-aware outbound event channel for attaching clients.
    pub(crate) events: Arc<ChatEventChannel>,
    /// Number of currently-attached WS adapters. The driver consults
    /// this to decide whether an idle run may be reaped.
    pub(crate) attach_count: Arc<AtomicUsize>,
    /// Explicit-stop signal. Cancelled by `POST /v1/run/:id/stop`; the
    /// driver watches it to tear down (cancelling any active turn).
    pub(crate) shutdown: CancellationToken,
}

/// `run_id` → live chat run.
pub(crate) type ChatRunRegistry = Arc<DashMap<String, Arc<ChatRunHandle>>>;

/// RAII guard that tracks one WS attach against a run's `attach_count`.
/// Incremented on construction, decremented on drop, so the driver's
/// idle-reaper sees an accurate live-attach count even if the adapter
/// task unwinds.
pub(crate) struct AttachGuard {
    count: Arc<AtomicUsize>,
}

impl AttachGuard {
    pub(crate) fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self { count }
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AssistantMessageStart, TextDelta};

    fn text(s: &str) -> OutboundMessage {
        OutboundMessage::TextDelta(TextDelta { text: s.to_string() })
    }

    #[test]
    fn subscribe_replays_history_in_order() {
        let ch = ChatEventChannel::new();
        ch.push(text("a"));
        ch.push(text("b"));

        let sub = ch.subscribe();
        let replayed: Vec<String> = sub
            .history
            .iter()
            .map(|m| match m {
                OutboundMessage::TextDelta(d) => d.text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(replayed, vec!["a".to_string(), "b".to_string()]);
        assert!(!sub.already_done);
    }

    #[test]
    fn history_caps_at_capacity_dropping_oldest() {
        let ch = ChatEventChannel::new();
        for i in 0..(CHAT_EVENT_HISTORY_CAPACITY + 5) {
            ch.push(text(&i.to_string()));
        }
        let sub = ch.subscribe();
        assert_eq!(sub.history.len(), CHAT_EVENT_HISTORY_CAPACITY);
        // Oldest five evicted: first retained entry is "5".
        match &sub.history[0] {
            OutboundMessage::TextDelta(d) => assert_eq!(d.text, "5"),
            other => panic!("unexpected first entry: {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_delivers_live_events_once() {
        let ch = ChatEventChannel::new();
        ch.push(text("history"));

        let mut sub = ch.subscribe();
        assert_eq!(sub.history.len(), 1, "pre-subscribe event lands in history");

        ch.push(text("live"));
        let got = sub.live.recv().await.expect("live event delivered");
        match got {
            OutboundMessage::TextDelta(d) => assert_eq!(d.text, "live"),
            other => panic!("unexpected live event: {other:?}"),
        }
        // The history event must NOT also arrive on the live receiver
        // (exactly-once across history + live).
        assert!(
            sub.live.try_recv().is_err(),
            "no duplicate delivery of the replayed history event"
        );
    }

    #[tokio::test]
    async fn forwarder_drains_into_history_and_live() {
        let ch = ChatEventChannel::new();
        let tx = spawn_event_forwarder(ch.clone());

        tx.send(OutboundMessage::AssistantMessageStart(AssistantMessageStart {
            message_id: "m1".into(),
        }))
        .await
        .unwrap();

        // Give the forwarder a tick to drain.
        for _ in 0..50 {
            if !ch.subscribe().history.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let sub = ch.subscribe();
        assert_eq!(sub.history.len(), 1);
        match &sub.history[0] {
            OutboundMessage::AssistantMessageStart(s) => assert_eq!(s.message_id, "m1"),
            other => panic!("unexpected forwarded entry: {other:?}"),
        }
    }

    #[test]
    fn mark_done_sets_already_done() {
        let ch = ChatEventChannel::new();
        assert!(!ch.subscribe().already_done);
        ch.mark_done();
        assert!(ch.subscribe().already_done);
    }

    #[test]
    fn attach_guard_tracks_count() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let _g1 = AttachGuard::new(count.clone());
            assert_eq!(count.load(Ordering::Acquire), 1);
            {
                let _g2 = AttachGuard::new(count.clone());
                assert_eq!(count.load(Ordering::Acquire), 2);
            }
            assert_eq!(count.load(Ordering::Acquire), 1);
        }
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
