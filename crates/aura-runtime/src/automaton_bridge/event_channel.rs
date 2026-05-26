//! Replay-aware broadcast wrapper for automaton events.
//!
//! Splits cleanly out of `automaton_bridge` because every state and
//! method here is about one thing: making sure a late WebSocket
//! subscriber to a fast-terminating automaton can still observe the
//! events emitted before it finished its handshake. The motivating
//! incident — `aura-os-server`'s WS client connecting to
//! `/stream/automaton/:id` *after* `POST /automaton/start` returned
//! and seeing an empty stream — is described on [`EventChannel`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aura_automaton::AutomatonEvent;
use parking_lot::Mutex;
use tokio::sync::broadcast;

use super::AutomatonBridge;

pub(super) const EVENT_BROADCAST_CAPACITY: usize = 512;

/// Cap on the per-automaton replay buffer used by [`EventChannel::history`].
/// Mirrors the broadcast ring so an existing subscriber that manages to
/// keep up and a late subscriber that relies on replay see the same
/// visible window. Exceeding the cap drops the oldest entries first.
pub(super) const EVENT_HISTORY_CAPACITY: usize = EVENT_BROADCAST_CAPACITY;

/// How long an [`EventChannel`] is kept in [`AutomatonBridge::event_channels`]
/// after the automaton emits `Done`. Provides a grace window for late
/// WebSocket subscribers (in particular, aura-os-server connects to
/// `/stream/automaton/:id` *after* `POST /automaton/start` returns, and
/// a fast-failing automaton can emit all its events before the WS
/// client even finishes its handshake). During this window
/// `subscribe_events` still returns the full replay history so the
/// late subscriber can reconstruct the task's outcome.
pub(super) const RETENTION_AFTER_DONE: Duration = Duration::from_secs(300);

/// Per-automaton event bus.
///
/// The raw `broadcast::Sender` we used previously had a subtle race:
/// `tokio::sync::broadcast` only delivers to receivers that existed
/// when `send` was called. A new subscriber joining after emission
/// starts at the tail and misses every event already sent, including
/// `Started` / `TaskStarted` / `TaskFailed` / `TaskCompleted` / `Done`.
/// For fast-terminating automatons (typical failure paths complete in
/// <100 ms) the aura-os-server WS client would therefore connect to a
/// "stream closed before terminal event arrived" - no visible reason,
/// no task outcome - even though the harness logs showed the automaton
/// had in fact run and failed.
///
/// This wrapper bundles a `broadcast::Sender` with a replay `history`
/// buffer: `spawn_event_forwarder` appends every event to `history`
/// before broadcasting, and `subscribe_events` returns the history
/// snapshot alongside a live receiver. Late subscribers get the full
/// event sequence regardless of when they joined.
pub(crate) struct EventChannel {
    /// Replay history. Capped at [`EVENT_HISTORY_CAPACITY`]; when full,
    /// the oldest entries are dropped. Cloned on each `subscribe_events`
    /// call (single-automaton events are small serde-derived values so
    /// the clone is cheap relative to the ~300s retention window).
    pub(super) history: Mutex<Vec<AutomatonEvent>>,
    /// Live broadcast for in-flight subscribers. Retained inside the
    /// `Arc<EventChannel>` so the sender outlives the forwarder task
    /// and late subscribers don't see `RecvError::Closed` before they've
    /// drained the history.
    pub(super) broadcast: broadcast::Sender<AutomatonEvent>,
    /// Set once the forwarder has observed and forwarded
    /// `AutomatonEvent::Done`. Lets subscribers skip the live-receive
    /// loop entirely when the automaton has already finished.
    pub(super) done: AtomicBool,
}

/// Snapshot returned by [`AutomatonBridge::subscribe_events`]. Gives
/// callers both the replay history (consume first, in order) and a
/// live receiver (consume next, in order) so they produce the same
/// ordering any early subscriber would have seen.
pub struct EventSubscription {
    /// All events the automaton has emitted so far, in emission order.
    /// May be empty if the automaton hasn't ticked yet, or capped at
    /// [`EVENT_HISTORY_CAPACITY`] for long-lived dev-loop automatons.
    pub history: Vec<AutomatonEvent>,
    /// Receiver for events emitted after this subscribe call. Will
    /// yield `RecvError::Closed` once the retention window elapses
    /// (or immediately, if `already_done` is true and no more events
    /// will ever be sent).
    pub live: broadcast::Receiver<AutomatonEvent>,
    /// True when `Done` is already in `history`. Callers can use this
    /// to avoid waiting on `live.recv()` after draining history.
    pub already_done: bool,
}

impl AutomatonBridge {
    /// Spawn a background task that forwards `mpsc` events from the
    /// automaton runtime into both the replay `history` buffer and
    /// the live broadcast. See [`EventChannel`] for why both paths
    /// are needed.
    ///
    /// After `Done` is forwarded the channel entry is kept alive for
    /// [`RETENTION_AFTER_DONE`] so late subscribers can still pull
    /// the replay history. The entry is removed from
    /// [`AutomatonBridge::event_channels`] at the end of that window.
    ///
    /// # Drain semantics
    ///
    /// The forwarder keeps polling `event_rx.recv()` until **all
    /// senders are dropped** (i.e. `recv()` returns `None`), even
    /// after observing `AutomatonEvent::Done`. The pre-fix loop
    /// `break`-ed the moment `Done` arrived and then slept on the
    /// retention timer with `event_rx` still in scope but no longer
    /// polled; if anything emitted a late protocol event during the
    /// 300s retention window the channel buffer would fill up and
    /// then close, producing the `TickContext::emit ... receiver
    /// closed` warnings observed in production for `TaskCompleted`
    /// / `TokenUsage` / `TaskStarted` / `TaskFailed`. Draining
    /// until exhaustion keeps the receiver alive for the full
    /// senders-alive window and forwards every late event into both
    /// the history buffer and the live broadcast.
    pub(super) fn spawn_event_forwarder(
        &self,
        automaton_id: String,
        mut event_rx: tokio::sync::mpsc::Receiver<AutomatonEvent>,
    ) -> Arc<EventChannel> {
        let (broadcast_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let channel = Arc::new(EventChannel {
            history: Mutex::new(Vec::new()),
            broadcast: broadcast_tx,
            done: AtomicBool::new(false),
        });
        let channels = self.event_channels.clone();
        channels.insert(automaton_id.clone(), channel.clone());

        let channel_for_task = channel.clone();
        let id_for_task = automaton_id.clone();
        tokio::spawn(async move {
            let mut done_observed_at: Option<tokio::time::Instant> = None;
            loop {
                // After `Done` is observed, keep draining for at
                // most `RETENTION_AFTER_DONE` so late protocol events
                // emitted in the same tick (or by clean-up tasks) are
                // still forwarded. Once the retention window elapses
                // we break and let `event_rx` drop, closing the
                // channel for any further senders.
                let recv = match done_observed_at {
                    None => event_rx.recv().await,
                    Some(started) => {
                        let remaining = RETENTION_AFTER_DONE
                            .checked_sub(started.elapsed())
                            .unwrap_or(std::time::Duration::ZERO);
                        if remaining.is_zero() {
                            break;
                        }
                        match tokio::time::timeout(remaining, event_rx.recv()).await {
                            Ok(v) => v,
                            Err(_) => break,
                        }
                    }
                };
                let Some(event) = recv else { break };

                let is_done = matches!(event, AutomatonEvent::Done);
                // Append to the replay history BEFORE broadcasting so
                // a subscriber that manages to subscribe between the
                // two operations sees the event in its history rather
                // than missing it entirely. Cap with
                // EVENT_HISTORY_CAPACITY (oldest-first eviction).
                {
                    let mut history = channel_for_task.history.lock();
                    if history.len() >= EVENT_HISTORY_CAPACITY {
                        let drop_n = history.len() + 1 - EVENT_HISTORY_CAPACITY;
                        history.drain(..drop_n);
                    }
                    history.push(event.clone());
                }
                let _ = channel_for_task.broadcast.send(event);
                if is_done && done_observed_at.is_none() {
                    channel_for_task.done.store(true, Ordering::Release);
                    done_observed_at = Some(tokio::time::Instant::now());
                }
            }

            channels.remove(&id_for_task);
        });

        channel
    }
}
