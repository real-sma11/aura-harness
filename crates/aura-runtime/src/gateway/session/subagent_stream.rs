//! Runtime-layer subagent observability: make each `task` child run
//! observable as its own live WS-attachable thread.
//!
//! Layer boundary: this module is the ONLY place that turns subagent
//! activity into `aura_protocol::OutboundMessage` frames and registers
//! a child run id in the [`super::chat_run::ChatRunRegistry`]. The
//! fleet (`aura-fleet-spawn` / `aura-fleet-subagent`) and engine
//! (`aura-engine`) layers only forward an opaque
//! [`AgentLoopEvent`](aura_agent::AgentLoopEvent) sink; they never
//! construct wire messages.
//!
//! Flow per `task` dispatch:
//! 1. Mint a `child_run_id` (UUID).
//! 2. Register an event-only [`super::chat_run::ChatRunHandle`] under
//!    that id so `WS /stream/:child_run_id` can attach (replay + live).
//! 3. Spawn a forwarder mapping the child loop's `AgentLoopEvent`s onto
//!    the child run's `OutboundMessage` stream
//!    ([`super::helpers::forward_events_to_ws`]).
//! 4. Emit `SubagentSpawned` on the PARENT stream BEFORE the (blocking
//!    Wait) dispatch.
//! 5. Delegate to the inner [`EventAwareSubagentDispatch`], threading
//!    the child event sink + the parent turn cancellation token.
//! 6. On completion, emit `SubagentStatus` on the parent stream, push a
//!    terminal status into the child stream, and schedule the child run
//!    for reaping after the idle-retention grace window.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use async_trait::async_trait;
use aura_agent::AgentLoopEvent;
use aura_core_types::{SubagentDispatchRequest, SubagentExit, SubagentResult};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_tools::SubagentDispatchHook;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::chat_run::{
    spawn_event_forwarder, ChatEventChannel, ChatRunHandle, ChatRunRegistry,
    CHAT_RUN_IDLE_RETENTION,
};
use crate::protocol::{InboundMessage, OutboundMessage, SubagentSpawned, SubagentStatus};

/// Capacity of the per-child [`AgentLoopEvent`] channel. Mirrors the
/// parent turn channel in [`super::chat::dispatch_turn_to_agent`].
const CHILD_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Capacity of the event-only run's (unused) inbound command channel.
const CHILD_COMMAND_CHANNEL_CAPACITY: usize = 8;

/// Observability-aware dispatch surface implemented by
/// [`FleetSubagentDispatcher`]. Carved out as a trait so the runtime
/// hook can wrap a test double without standing up a full fleet
/// spawner.
#[async_trait]
pub(crate) trait EventAwareSubagentDispatch: Send + Sync {
    /// Dispatch the child run, threading an optional streaming sink and
    /// the parent turn cancellation token.
    async fn dispatch_with_events(
        &self,
        request: SubagentDispatchRequest,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancellation: Option<CancellationToken>,
    ) -> Result<SubagentResult, String>;
}

#[async_trait]
impl EventAwareSubagentDispatch for FleetSubagentDispatcher {
    async fn dispatch_with_events(
        &self,
        request: SubagentDispatchRequest,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancellation: Option<CancellationToken>,
    ) -> Result<SubagentResult, String> {
        FleetSubagentDispatcher::spawn_with_events(self, request, event_tx, cancellation).await
    }
}

/// Wraps an inner subagent dispatcher and makes each child run
/// observable on the wire. See the module docs for the per-dispatch
/// flow.
pub(crate) struct RuntimeSubagentObservabilityHook {
    inner: Arc<dyn EventAwareSubagentDispatch>,
    parent_outbound: mpsc::Sender<OutboundMessage>,
    chat_runs: ChatRunRegistry,
    parent_cancellation: Option<CancellationToken>,
}

impl RuntimeSubagentObservabilityHook {
    /// Construct a hook that emits onto `parent_outbound`, registers
    /// child runs in `chat_runs`, and forks `parent_cancellation` into
    /// each `Wait` child.
    pub(crate) fn new(
        inner: Arc<dyn EventAwareSubagentDispatch>,
        parent_outbound: mpsc::Sender<OutboundMessage>,
        chat_runs: ChatRunRegistry,
        parent_cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            inner,
            parent_outbound,
            chat_runs,
            parent_cancellation,
        }
    }

    /// Register an event-only child run under `child_run_id` and return
    /// the [`AgentLoopEvent`] sink the child loop streams into, plus the
    /// run's event channel (so the caller can push a terminal status and
    /// mark it done).
    fn register_child_run(
        &self,
        child_run_id: &str,
    ) -> (
        mpsc::Sender<AgentLoopEvent>,
        Arc<ChatEventChannel>,
        tokio::task::JoinHandle<()>,
    ) {
        let channel = ChatEventChannel::new();
        let child_outbound = spawn_event_forwarder(channel.clone());

        // Event-only run: no driver consumes inbound commands, so the
        // receiver is dropped immediately. A WS attach can still replay
        // history + stream live; inbound frames simply close that
        // attach's reader.
        let (commands, _commands_rx) =
            mpsc::channel::<InboundMessage>(CHILD_COMMAND_CHANNEL_CAPACITY);
        let handle = Arc::new(ChatRunHandle {
            commands,
            events: channel.clone(),
            attach_count: Arc::new(AtomicUsize::new(0)),
            shutdown: CancellationToken::new(),
        });
        self.chat_runs.insert(child_run_id.to_string(), handle);

        let (event_tx, event_rx) = mpsc::channel::<AgentLoopEvent>(CHILD_EVENT_CHANNEL_CAPACITY);
        // The forwarder ends when the child loop drops its event sender,
        // i.e. when the child run finishes — the detached path awaits this
        // handle to emit a terminal status without holding the child's
        // `result_rx` (owned by the spawner).
        let forwarder = tokio::spawn(super::helpers::forward_events_to_ws(
            event_rx,
            child_outbound,
        ));

        (event_tx, channel, forwarder)
    }
}

#[async_trait]
impl SubagentDispatchHook for RuntimeSubagentObservabilityHook {
    async fn dispatch(&self, request: SubagentDispatchRequest) -> Result<SubagentResult, String> {
        let child_run_id = Uuid::new_v4().to_string();
        let parent_tool_use_id = request.tool_call_id.clone();
        let subagent_type = request.subagent_type.clone();
        let prompt = request.prompt.clone();
        let is_detached = matches!(
            request.spawn_mode,
            Some(aura_core_types::SpawnMode::Detached)
        );

        let (event_tx, child_channel, forwarder) = self.register_child_run(&child_run_id);

        // Emit `SubagentSpawned` on the parent stream BEFORE the (Wait)
        // dispatch blocks so the client can render a clickable thread
        // card and lazily attach to `child_run_id`.
        let _ = self
            .parent_outbound
            .try_send(OutboundMessage::SubagentSpawned(SubagentSpawned {
                child_run_id: child_run_id.clone(),
                parent_tool_use_id,
                subagent_type,
                prompt,
            }));

        let result = self
            .inner
            .dispatch_with_events(request, Some(event_tx), self.parent_cancellation.clone())
            .await;

        // Detached dispatch returns an immediate ack while the child is
        // still running in the background. Reflect `running` now and emit
        // the terminal status when the child's event stream closes — NOT
        // a premature `completed` derived from the ack.
        if is_detached && result.is_ok() {
            let running = SubagentStatus {
                child_run_id: child_run_id.clone(),
                state: "running".to_string(),
                reason: None,
            };
            let _ = self
                .parent_outbound
                .try_send(OutboundMessage::SubagentStatus(running.clone()));
            child_channel.push(OutboundMessage::SubagentStatus(running));
            spawn_detached_completion_watch(
                forwarder,
                self.parent_outbound.clone(),
                child_channel,
                self.chat_runs.clone(),
                child_run_id,
            );
            return result;
        }

        let status = status_payload(&child_run_id, &result);
        let _ = self
            .parent_outbound
            .try_send(OutboundMessage::SubagentStatus(status.clone()));

        // Terminate the child stream cleanly: push the terminal status
        // into its replay history, then mark it done so a late attach
        // replays the full thread without blocking on a live receiver.
        child_channel.push(OutboundMessage::SubagentStatus(status));
        child_channel.mark_done();
        // Wait path: the forwarder is already draining to completion as
        // the child loop dropped its sender; detaching the handle is
        // equivalent to the prior fire-and-forget spawn.
        drop(forwarder);
        schedule_child_run_cleanup(self.chat_runs.clone(), child_run_id);

        result
    }
}

/// Watch a detached child's event forwarder to completion, then publish
/// the terminal status on both the parent and child streams and reap the
/// run. The typed exit is not available here (the spawner owns the
/// detached `result_rx`), so a generic `completed` terminal is emitted;
/// any failure detail still streams through the child's live events.
fn spawn_detached_completion_watch(
    forwarder: tokio::task::JoinHandle<()>,
    parent_outbound: mpsc::Sender<OutboundMessage>,
    child_channel: Arc<ChatEventChannel>,
    chat_runs: ChatRunRegistry,
    child_run_id: String,
) {
    tokio::spawn(async move {
        let _ = forwarder.await;
        let status = SubagentStatus {
            child_run_id: child_run_id.clone(),
            state: "completed".to_string(),
            reason: None,
        };
        let _ = parent_outbound.try_send(OutboundMessage::SubagentStatus(status.clone()));
        child_channel.push(OutboundMessage::SubagentStatus(status));
        child_channel.mark_done();
        schedule_child_run_cleanup(chat_runs, child_run_id);
    });
}

/// Map a dispatch outcome onto the wire `SubagentStatus` payload. The
/// `state` string matches the documented vocabulary on
/// [`SubagentStatus`]: `running | completed | failed | cancelled |
/// timeout | rejected`.
fn status_payload(child_run_id: &str, result: &Result<SubagentResult, String>) -> SubagentStatus {
    let (state, reason) = match result {
        Ok(res) => match &res.exit {
            SubagentExit::Completed => ("completed", None),
            SubagentExit::Cancelled => ("cancelled", None),
            SubagentExit::Timeout => ("timeout", None),
            SubagentExit::Failed { reason } => ("failed", Some(reason.clone())),
            SubagentExit::Rejected { reason } => ("rejected", Some(reason.clone())),
        },
        Err(err) => ("failed", Some(err.clone())),
    };
    SubagentStatus {
        child_run_id: child_run_id.to_string(),
        state: state.to_string(),
        reason,
    }
}

/// Reap the event-only child run from the registry after the idle
/// retention window so a client that attached mid-run can still replay
/// the completed thread, but the entry does not leak forever.
fn schedule_child_run_cleanup(chat_runs: ChatRunRegistry, child_run_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(CHAT_RUN_IDLE_RETENTION).await;
        chat_runs.remove(&child_run_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::time::Duration;

    /// Test double: records the streamed event channel and replays a
    /// canned [`AgentLoopEvent`] before returning the configured exit.
    struct StubDispatch {
        exit: SubagentExit,
        streamed_text: Option<String>,
    }

    #[async_trait]
    impl EventAwareSubagentDispatch for StubDispatch {
        async fn dispatch_with_events(
            &self,
            _request: SubagentDispatchRequest,
            event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
            _cancellation: Option<CancellationToken>,
        ) -> Result<SubagentResult, String> {
            if let (Some(tx), Some(text)) = (event_tx.as_ref(), self.streamed_text.as_ref()) {
                let _ = tx.send(AgentLoopEvent::TextDelta(text.clone())).await;
            }
            Ok(SubagentResult {
                child_agent_id: None,
                final_message: "done".into(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: self.exit.clone(),
            })
        }
    }

    fn request() -> SubagentDispatchRequest {
        SubagentDispatchRequest {
            parent_agent_id: aura_core_types::AgentId::generate(),
            subagent_type: "explore".into(),
            prompt: "investigate".into(),
            originating_user_id: None,
            parent_chain: Vec::new(),
            model_override: None,
            system_prompt_addendum: None,
            parent_permissions: aura_core_types::AgentPermissions::empty(),
            parent_tool_permissions: None,
            user_tool_defaults: aura_core_types::UserToolDefaults::full_access(),
            tool_call_id: Some("toolu_parent_1".into()),
            parent_mode: None,
            parent_kernel_mode: None,
            parent_model_id: None,
            override_mode: None,
            override_permissions: None,
            override_tool_subset: None,
            override_isolation_id: None,
            override_budget: None,
            spawn_mode: None,
        }
    }

    fn hook_with(
        stub: StubDispatch,
    ) -> (
        RuntimeSubagentObservabilityHook,
        mpsc::Receiver<OutboundMessage>,
        ChatRunRegistry,
    ) {
        let (parent_tx, parent_rx) = mpsc::channel::<OutboundMessage>(16);
        let registry: ChatRunRegistry = Arc::new(DashMap::new());
        let hook = RuntimeSubagentObservabilityHook::new(
            Arc::new(stub),
            parent_tx,
            registry.clone(),
            Some(CancellationToken::new()),
        );
        (hook, parent_rx, registry)
    }

    /// A successful dispatch emits `SubagentSpawned` (with the parent
    /// tool-use id wired through) followed by `SubagentStatus`
    /// (`completed`) on the parent stream, registers the minted child
    /// run id, and the child stream replays the streamed text frame so
    /// an attaching client receives live events.
    #[tokio::test]
    async fn spawn_then_completed_status_and_child_stream_is_attachable() {
        let (hook, mut parent_rx, registry) = hook_with(StubDispatch {
            exit: SubagentExit::Completed,
            streamed_text: Some("hello from child".into()),
        });

        let result = hook.dispatch(request()).await.expect("dispatch ok");
        assert_eq!(result.final_message, "done");

        // Parent stream: SubagentSpawned first, then SubagentStatus.
        let child_run_id = match parent_rx.recv().await.expect("spawned frame") {
            OutboundMessage::SubagentSpawned(spawned) => {
                assert_eq!(spawned.subagent_type, "explore");
                assert_eq!(spawned.prompt, "investigate");
                assert_eq!(
                    spawned.parent_tool_use_id.as_deref(),
                    Some("toolu_parent_1")
                );
                spawned.child_run_id
            }
            other => panic!("expected SubagentSpawned, got {other:?}"),
        };
        match parent_rx.recv().await.expect("status frame") {
            OutboundMessage::SubagentStatus(status) => {
                assert_eq!(status.child_run_id, child_run_id);
                assert_eq!(status.state, "completed");
                assert!(status.reason.is_none());
            }
            other => panic!("expected SubagentStatus, got {other:?}"),
        }

        // The child run id is attachable via the chat-run registry.
        let handle = registry.get(&child_run_id).expect("child run registered");
        let events = handle.events.clone();
        drop(handle);

        // The child stream replays the streamed text frame (forwarded
        // from the child loop's AgentLoopEvent) plus the terminal status.
        let mut saw_text = false;
        for _ in 0..200 {
            let sub = events.subscribe();
            saw_text = sub.history.iter().any(|m| {
                matches!(m, OutboundMessage::TextDelta(delta) if delta.text == "hello from child")
            });
            if saw_text {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            saw_text,
            "child stream should replay the forwarded text frame"
        );
        assert!(
            events
                .subscribe()
                .history
                .iter()
                .any(|m| matches!(m, OutboundMessage::SubagentStatus(_))),
            "child stream should carry the terminal status"
        );
        assert!(events.subscribe().already_done, "child stream marked done");
    }

    /// A rejected dispatch (e.g. depth/quota) surfaces as
    /// `SubagentStatus { state: "rejected", reason: Some(..) }` on the
    /// parent stream.
    #[tokio::test]
    async fn rejected_dispatch_reports_reason_on_parent_stream() {
        let (hook, mut parent_rx, _registry) = hook_with(StubDispatch {
            exit: SubagentExit::Rejected {
                reason: "depth exceeded".into(),
            },
            streamed_text: None,
        });

        let _ = hook.dispatch(request()).await.expect("dispatch ok");

        assert!(matches!(
            parent_rx.recv().await,
            Some(OutboundMessage::SubagentSpawned(_))
        ));
        match parent_rx.recv().await.expect("status frame") {
            OutboundMessage::SubagentStatus(status) => {
                assert_eq!(status.state, "rejected");
                assert_eq!(status.reason.as_deref(), Some("depth exceeded"));
            }
            other => panic!("expected SubagentStatus, got {other:?}"),
        }
    }

    /// A detached dispatch must reflect `running` immediately (not a
    /// premature `completed` from the ack), then emit a terminal
    /// `completed` once the child's event stream closes.
    #[tokio::test]
    async fn detached_dispatch_reports_running_then_completed() {
        let (hook, mut parent_rx, _registry) = hook_with(StubDispatch {
            exit: SubagentExit::Completed,
            streamed_text: Some("working".into()),
        });

        let mut detached = request();
        detached.spawn_mode = Some(aura_core_types::SpawnMode::Detached);
        let _ = hook.dispatch(detached).await.expect("dispatch ok");

        assert!(matches!(
            parent_rx.recv().await,
            Some(OutboundMessage::SubagentSpawned(_))
        ));
        match parent_rx.recv().await.expect("running status frame") {
            OutboundMessage::SubagentStatus(status) => {
                assert_eq!(status.state, "running");
                assert!(status.reason.is_none());
            }
            other => panic!("expected running SubagentStatus, got {other:?}"),
        }
        // The completion watch fires once the forwarder drains (the stub
        // dropped its event sender after returning the ack).
        match parent_rx.recv().await.expect("terminal status frame") {
            OutboundMessage::SubagentStatus(status) => {
                assert_eq!(status.state, "completed");
            }
            other => panic!("expected completed SubagentStatus, got {other:?}"),
        }
    }
}
