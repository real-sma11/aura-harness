//! Reattachable chat-run registry and per-run event channel.
//!
//! Mirrors the automaton-side
//! [`aura_engine::automaton::EventChannel`] model so a chat run's
//! turn execution can be decoupled from any single WebSocket
//! connection. Where the automaton channel replays
//! [`aura_surface_automaton::AutomatonEvent`]s, this one replays
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

use super::{Session, WsContext};
use crate::protocol::{ErrorMsg, InboundMessage, OutboundMessage};

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

/// Spawn a [`super::chat::run_chat_driver`] task that owns `session`
/// and its turn execution, register it under `run_id`, and return the
/// shared [`ChatRunHandle`]. The driver runs independently of any
/// WebSocket; it removes itself from the registry when it stops.
pub(crate) fn spawn_chat_run(
    session: Session,
    ctx: WsContext,
    run_id: String,
    registry: ChatRunRegistry,
) -> Arc<ChatRunHandle> {
    let events = ChatEventChannel::new();
    let outbound_tx = spawn_event_forwarder(events.clone());
    let (commands_tx, commands_rx) = mpsc::channel::<InboundMessage>(256);
    let attach_count = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();

    let handle = Arc::new(ChatRunHandle {
        commands: commands_tx,
        events: events.clone(),
        attach_count: attach_count.clone(),
        shutdown: shutdown.clone(),
    });
    registry.insert(run_id.clone(), handle.clone());

    let registry_for_task = registry.clone();
    tokio::spawn(async move {
        super::chat::run_chat_driver(
            session,
            ctx,
            events,
            outbound_tx,
            commands_rx,
            attach_count,
            shutdown,
        )
        .await;
        // Driver stopped: drop the registry entry so no new attach finds
        // a dead run. Clients that already subscribed keep their (now
        // `done`) channel until they drop.
        registry_for_task.remove(&run_id);
    });

    handle
}

/// Thin WS adapter for a chat run: replay the run's history, then
/// stream live events, while forwarding inbound frames to the driver's
/// command channel. Multiple concurrent attaches to the same run are
/// allowed. A WS close ends only this attach — it never cancels the
/// turn (plan Part C).
pub(crate) async fn handle_chat_ws_attach(
    socket: axum::extract::ws::WebSocket,
    handle: Arc<ChatRunHandle>,
    run_id: String,
) {
    use futures_util::StreamExt;

    let (mut ws_tx, mut ws_rx) = socket.split();
    // Track this attach for the driver's idle-reaper.
    let _attach = AttachGuard::new(handle.attach_count.clone());
    let ChatEventSubscription {
        history,
        mut live,
        already_done,
    } = handle.events.subscribe();

    tracing::info!(
        run_id = %run_id,
        history_len = history.len(),
        already_done,
        "Chat run WS attached"
    );

    // Per-attach control channel: lets the reader push frames addressed
    // to *this* socket only (e.g. a `parse_error` for malformed inbound
    // JSON) without broadcasting them into the run's shared replay
    // history, which every other attached client would otherwise see.
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<OutboundMessage>(8);

    // Reader task: forward inbound frames to the driver. On WS close it
    // signals `attach_closed` (so the writer stops) but does NOT cancel
    // the turn.
    let attach_closed = CancellationToken::new();
    let attach_closed_reader = attach_closed.clone();
    let commands = handle.commands.clone();
    let read_run_id = run_id.clone();
    let reader = tokio::spawn(async move {
        use axum::extract::ws::Message as WsMessage;
        while let Some(frame) = ws_rx.next().await {
            match frame {
                Ok(WsMessage::Text(text)) => {
                    match serde_json::from_str::<InboundMessage>(&text) {
                        Ok(inbound) => {
                            if commands.send(inbound).await.is_err() {
                                // Driver gone; nothing left to forward.
                                break;
                            }
                        }
                        Err(e) => {
                            crate::inbound_console::ws_rejection_line(
                                "framing",
                                "parse_error",
                                Some(&format!("run_id={read_run_id} {e}")),
                            );
                            // Tell *this* client its frame was rejected.
                            // Recoverable: the socket stays open so the
                            // client can resend a well-formed message.
                            if ctrl_tx
                                .send(OutboundMessage::Error(ErrorMsg {
                                    code: "parse_error".into(),
                                    message: format!("failed to parse inbound message: {e}"),
                                    recoverable: true,
                                    support_id: None,
                                }))
                                .await
                                .is_err()
                            {
                                // Writer gone; nothing left to notify.
                                break;
                            }
                        }
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        attach_closed_reader.cancel();
    });

    // Writer: replay history first (exactly the messages an early
    // attach would have seen), then live until the WS closes or the run
    // terminates.
    let mut closed = false;
    for msg in history {
        if send_outbound_frame(&mut ws_tx, &msg).await.is_err() {
            closed = true;
            break;
        }
    }

    if !closed && !already_done {
        loop {
            tokio::select! {
                biased;
                () = attach_closed.cancelled() => break,
                // Per-attach control frames (e.g. `parse_error`) go only
                // to this socket, ahead of live broadcast traffic.
                Some(ctrl) = ctrl_rx.recv() => {
                    if send_outbound_frame(&mut ws_tx, &ctrl).await.is_err() {
                        break;
                    }
                }
                recv = live.recv() => match recv {
                    Ok(msg) => {
                        if send_outbound_frame(&mut ws_tx, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let warn = OutboundMessage::Progress(crate::protocol::ProgressMsg {
                            stage: "lagged".into(),
                            tool_name: None,
                            elapsed_ms: None,
                            message: Some(format!("dropped {n} messages (client too slow)")),
                        });
                        let _ = send_outbound_frame(&mut ws_tx, &warn).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }

    reader.abort();
    tracing::info!(run_id = %run_id, "Chat run WS attach ended");
}

/// Serialize and send one outbound message over the WS sink. Returns
/// `Err(())` only when the socket itself failed (so the writer should
/// stop); a serialization failure is logged and skipped.
async fn send_outbound_frame<S>(sink: &mut S, msg: &OutboundMessage) -> Result<(), ()>
where
    S: futures_util::Sink<axum::extract::ws::Message> + Unpin,
{
    use axum::extract::ws::Message as WsMessage;
    use futures_util::SinkExt;
    match serde_json::to_string(msg) {
        Ok(json) => sink.send(WsMessage::Text(json)).await.map_err(|_| ()),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize outbound chat message");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AssistantMessageStart, TextDelta};

    fn text(s: &str) -> OutboundMessage {
        OutboundMessage::TextDelta(TextDelta {
            text: s.to_string(),
        })
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

        tx.send(OutboundMessage::AssistantMessageStart(
            AssistantMessageStart {
                message_id: "m1".into(),
            },
        ))
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

    /// Two concurrent subscribers (i.e. a live attach plus a reattach)
    /// both replay the full history and both then receive live events —
    /// the channel-level guarantee behind multi-attach (no 409) and
    /// reattach (history replay + live).
    #[tokio::test]
    async fn two_subscribers_both_replay_and_receive_live() {
        let ch = ChatEventChannel::new();
        ch.push(text("session_ready"));

        let mut first = ch.subscribe();
        // A second attach to the same run is non-destructive: it also
        // replays the existing history.
        let mut second = ch.subscribe();
        assert_eq!(first.history.len(), 1);
        assert_eq!(second.history.len(), 1);

        ch.push(text("live"));
        for sub in [&mut first, &mut second] {
            match sub.live.recv().await.expect("live event delivered") {
                OutboundMessage::TextDelta(d) => assert_eq!(d.text, "live"),
                other => panic!("unexpected live event: {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use crate::gateway::session::{prepare_chat_session, WsContext};
    use crate::protocol::{InboundMessage, OutboundMessage, UserMessage};
    use aura_engine::scheduler::Scheduler;
    use aura_model_reasoner::{MockProvider, ModelProvider};
    use aura_protocol::{
        AgentCapabilities, AgentIdentity, AgentPermissionsWire, ModelSelection, RuntimeRequest,
        RuntimeRequestType, WorkspaceLocation,
    };
    use aura_store_db::RocksStore;
    use aura_tools::{ToolCatalog, ToolConfig};

    fn driver_ctx() -> WsContext {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let db_dir = tempfile::tempdir().expect("temp db");
        let store = Arc::new(RocksStore::open(db_dir.path(), false).expect("open rocks store"));
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("ok"));
        let workspace_base = workspace.path().to_path_buf();
        let catalog = Arc::new(ToolCatalog::default());
        let scheduler = Arc::new(Scheduler::new(
            store.clone(),
            provider.clone(),
            Vec::new(),
            catalog.executor_builtin_tools(),
            workspace_base.clone(),
            None,
        ));
        std::mem::forget(workspace);
        std::mem::forget(db_dir);

        WsContext {
            workspace_base,
            provider,
            store,
            scheduler,
            tool_config: ToolConfig::default(),
            auth_token: None,
            catalog,
            domain_api: None,
            automaton_controller: None,
            project_base: None,
            memory_manager: None,
            skill_manager: None,
            router_url: None,
            aura_os_server_url: None,
            chat_runs: Arc::new(DashMap::new()),
        }
    }

    fn chat_request(workspace: String) -> RuntimeRequest {
        RuntimeRequest {
            r#type: RuntimeRequestType::Chat {
                conversation_messages: Vec::new(),
            },
            agent_identity: AgentIdentity::default(),
            model: ModelSelection::default(),
            workspace: WorkspaceLocation {
                workspace: Some(workspace),
                project_path: None,
                git_repo_url: None,
                git_branch: None,
            },
            project: None,
            agent_permissions: AgentPermissionsWire::default(),
            tool_permissions: None,
            agent_capabilities: AgentCapabilities::default(),
            auth_jwt: None,
            user_id: "user-test".to_string(),
        }
    }

    /// Poll the channel's replay history until `pred` matches one of its
    /// messages or the deadline elapses.
    async fn wait_for_history(
        events: &Arc<ChatEventChannel>,
        pred: impl Fn(&OutboundMessage) -> bool,
        label: &str,
    ) {
        for _ in 0..600 {
            if events.subscribe().history.iter().any(&pred) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {label}");
    }

    /// End-to-end (no model network): a run spawned via `spawn_chat_run`
    /// executes a turn, emits frames into its replay channel, survives
    /// across turns, and a fresh attach (reattach) replays the full
    /// history including the completed turn's `AssistantMessageEnd`.
    #[tokio::test]
    async fn driver_runs_turn_and_completed_history_is_reattachable() {
        let ctx = driver_ctx();
        let ws_path = ctx.workspace_base.join("drv");
        std::fs::create_dir_all(&ws_path).expect("workspace dir");
        let session = prepare_chat_session(chat_request(ws_path.display().to_string()), &ctx)
            .await
            .expect("prepare chat session");

        let registry: ChatRunRegistry = Arc::new(DashMap::new());
        let handle = spawn_chat_run(session, ctx, "run-1".to_string(), registry.clone());

        // Simulate one live WS attach so the idle-reaper never fires
        // mid-test (and to exercise attach-count keeping the run alive).
        let _attach = AttachGuard::new(handle.attach_count.clone());

        // First attach observes SessionReady.
        wait_for_history(
            &handle.events,
            |m| matches!(m, OutboundMessage::SessionReady(_)),
            "SessionReady",
        )
        .await;

        // Drive a turn.
        handle
            .commands
            .send(InboundMessage::UserMessage(UserMessage {
                content: "hi".into(),
                tool_hints: None,
                attachments: None,
            }))
            .await
            .expect("send user message");

        wait_for_history(
            &handle.events,
            |m| matches!(m, OutboundMessage::AssistantMessageEnd(_)),
            "AssistantMessageEnd",
        )
        .await;

        // Reattach: a brand-new subscribe replays the whole session,
        // including the now-completed turn. This is the core
        // reattachability guarantee — a dropped+reconnected socket
        // backfills the turn it missed.
        let reattach = handle.events.subscribe();
        assert!(
            reattach
                .history
                .iter()
                .any(|m| matches!(m, OutboundMessage::SessionReady(_))),
            "reattach replays SessionReady"
        );
        assert!(
            reattach
                .history
                .iter()
                .any(|m| matches!(m, OutboundMessage::AssistantMessageEnd(_))),
            "reattach replays the completed turn's AssistantMessageEnd"
        );

        // The run persists across turns (one run_id per session
        // lifetime): it is still registered and can run another turn.
        assert!(registry.contains_key("run-1"), "run stays registered");

        // Explicit stop tears the run down and deregisters it.
        handle.shutdown.cancel();
        for _ in 0..200 {
            if !registry.contains_key("run-1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !registry.contains_key("run-1"),
            "explicit stop deregisters the run"
        );
        assert!(
            handle.events.subscribe().already_done,
            "stopped run marks its channel done"
        );
    }
}
