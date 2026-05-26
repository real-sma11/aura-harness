use tokio::sync::mpsc;
use tracing::warn;

use crate::events::AutomatonEvent;
use crate::state::AutomatonState;
use crate::types::AutomatonId;
use crate::AutomatonError;

pub struct TickContext {
    pub automaton_id: AutomatonId,
    pub state: AutomatonState,
    pub event_tx: mpsc::Sender<AutomatonEvent>,
    pub config: serde_json::Value,
    pub workspace_root: Option<std::path::PathBuf>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl TickContext {
    pub fn new(
        automaton_id: AutomatonId,
        state: AutomatonState,
        event_tx: mpsc::Sender<AutomatonEvent>,
        config: serde_json::Value,
        workspace_root: Option<std::path::PathBuf>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            automaton_id,
            state,
            event_tx,
            config,
            workspace_root,
            shutdown,
        }
    }

    pub fn emit(&self, event: AutomatonEvent) -> Result<(), AutomatonError> {
        match self.event_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(event)) => {
                let kind = event_kind(&event);
                let is_protocol = is_protocol_event(&event);
                // Protocol events (`TaskStarted` / `TaskCompleted` /
                // `TaskFailed` / `TokenUsage` / `LoopFinished` /
                // `Stopped`) carry runtime-state-machine semantics —
                // downstream observers (aura-os-server's DoD gate,
                // operator UIs) reconstruct task outcomes from them.
                // Losing one silently was producing the "tasks completed
                // but UI never sees `TaskCompleted`" symptom in
                // production logs; log them at `error!` with the
                // event kind so they show up in any operator filter.
                // Advisory events (`TextDelta`, `ThinkingDelta`, …)
                // are higher-cadence UI updates and stay at `warn!`.
                if is_protocol {
                    tracing::error!(
                        automaton_id = %self.automaton_id,
                        event_kind = kind,
                        event = ?event,
                        "automaton event receiver closed; dropping PROTOCOL event \
                         without failing automaton — downstream observers will not \
                         see this lifecycle transition"
                    );
                } else {
                    warn!(
                        automaton_id = %self.automaton_id,
                        event_kind = kind,
                        event = ?event,
                        "automaton event receiver closed; dropping event without failing automaton"
                    );
                }
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(event)) => {
                Err(AutomatonError::EventDelivery(format!("channel full: {event:?}")))
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub fn cancellation_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.shutdown
    }
}

/// Short string identifying which `AutomatonEvent` variant we're
/// looking at — used as a grep-friendly `event_kind=...` field on
/// the closed-receiver warnings emitted by [`TickContext::emit`].
/// Centralised here so future event variants pick up sensible
/// defaults via the catch-all arm without touching every log site.
fn event_kind(event: &AutomatonEvent) -> &'static str {
    match event {
        AutomatonEvent::Started { .. } => "started",
        AutomatonEvent::Stopped { .. } => "stopped",
        AutomatonEvent::Paused { .. } => "paused",
        AutomatonEvent::Resumed { .. } => "resumed",
        AutomatonEvent::Error { .. } => "error",
        AutomatonEvent::TextDelta { .. } => "text_delta",
        AutomatonEvent::ThinkingDelta { .. } => "thinking_delta",
        AutomatonEvent::Progress { .. } => "progress",
        AutomatonEvent::ToolCallStarted { .. } => "tool_call_started",
        AutomatonEvent::ToolCallSnapshot { .. } => "tool_call_snapshot",
        AutomatonEvent::ToolCallCompleted { .. } => "tool_call_completed",
        AutomatonEvent::ToolResult { .. } => "tool_result",
        AutomatonEvent::TaskStarted { .. } => "task_started",
        AutomatonEvent::TaskCompleted { .. } => "task_completed",
        AutomatonEvent::TaskFailed { .. } => "task_failed",
        AutomatonEvent::CommitSkipped { .. } => "commit_skipped",
        AutomatonEvent::TaskRetrying { .. } => "task_retrying",
        AutomatonEvent::ToolCallRetrying { .. } => "tool_call_retrying",
        AutomatonEvent::ToolCallFailed { .. } => "tool_call_failed",
        AutomatonEvent::LoopFinished { .. } => "loop_finished",
        AutomatonEvent::SpecSaved { .. } => "spec_saved",
        AutomatonEvent::SpecsTitle { .. } => "specs_title",
        AutomatonEvent::SpecsSummary { .. } => "specs_summary",
        AutomatonEvent::BuildVerificationStarted => "build_verification_started",
        AutomatonEvent::BuildVerificationPassed => "build_verification_passed",
        AutomatonEvent::BuildVerificationFailed { .. } => "build_verification_failed",
        AutomatonEvent::TestVerificationStarted => "test_verification_started",
        AutomatonEvent::TestVerificationPassed => "test_verification_passed",
        AutomatonEvent::TestVerificationFailed { .. } => "test_verification_failed",
        AutomatonEvent::BuildFixAttempt { .. } => "build_fix_attempt",
        AutomatonEvent::TestFixAttempt { .. } => "test_fix_attempt",
        AutomatonEvent::FileOpsApplied { .. } => "file_ops_applied",
        AutomatonEvent::GitCommitted { .. } => "git_committed",
        AutomatonEvent::GitCommitFailed { .. } => "git_commit_failed",
        AutomatonEvent::GitPushed { .. } => "git_pushed",
        AutomatonEvent::GitPushFailed { .. } => "git_push_failed",
        AutomatonEvent::SessionRolledOver { .. } => "session_rolled_over",
        AutomatonEvent::TokenUsage { .. } => "token_usage",
        AutomatonEvent::MessageSaved { .. } => "message_saved",
        AutomatonEvent::AgentInstanceUpdated { .. } => "agent_instance_updated",
        AutomatonEvent::LogLine { .. } => "log_line",
        AutomatonEvent::Done => "done",
        AutomatonEvent::DebugLlmCall { .. } => "debug.llm_call",
        AutomatonEvent::DebugIteration { .. } => "debug.iteration",
        AutomatonEvent::DebugBlocker { .. } => "debug.blocker",
        AutomatonEvent::DebugRetry { .. } => "debug.retry",
    }
}

/// Protocol events carry runtime-state-machine semantics that
/// downstream observers (the aura-os-server DoD gate, operator UIs,
/// run-log forwarders) reconstruct task outcomes from. Silently
/// dropping one means a completed/failed task never reaches the
/// observer — exactly the symptom observed in production logs. The
/// list intentionally mirrors the events emitted directly via
/// `TickContext::emit` in `aura-automaton/src/builtins/dev_loop/tick.rs`
/// and `aura-automaton/src/builtins/task_run.rs`; advisory streaming
/// events (`TextDelta`, `ThinkingDelta`, `ToolCallSnapshot`, …) flow
/// through the `forward_event` debounce path instead.
fn is_protocol_event(event: &AutomatonEvent) -> bool {
    matches!(
        event,
        AutomatonEvent::Started { .. }
            | AutomatonEvent::Stopped { .. }
            | AutomatonEvent::Error { .. }
            | AutomatonEvent::TaskStarted { .. }
            | AutomatonEvent::TaskCompleted { .. }
            | AutomatonEvent::TaskFailed { .. }
            | AutomatonEvent::TaskRetrying { .. }
            | AutomatonEvent::CommitSkipped { .. }
            | AutomatonEvent::LoopFinished { .. }
            | AutomatonEvent::TokenUsage { .. }
            | AutomatonEvent::Done
    )
}

#[cfg(test)]
mod tests {
    use super::TickContext;
    use crate::events::AutomatonEvent;
    use crate::state::AutomatonState;
    use crate::types::AutomatonId;
    use crate::AutomatonError;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn test_context(channel_size: usize) -> (TickContext, mpsc::Receiver<AutomatonEvent>) {
        let (tx, rx) = mpsc::channel(channel_size);
        let ctx = TickContext::new(
            AutomatonId::from_string("test-automaton"),
            AutomatonState::new(),
            tx,
            json!({}),
            None,
            CancellationToken::new(),
        );
        (ctx, rx)
    }

    #[test]
    fn emit_delivers_event_when_capacity_exists() {
        let (ctx, mut rx) = test_context(1);

        ctx.emit(AutomatonEvent::LogLine {
            message: "hello".to_string(),
        })
        .expect("emit event");

        assert!(matches!(
            rx.try_recv(),
            Ok(AutomatonEvent::LogLine { message }) if message == "hello"
        ));
    }

    #[test]
    fn emit_returns_structured_error_when_channel_is_full() {
        let (ctx, mut rx) = test_context(1);
        ctx.emit(AutomatonEvent::LogLine {
            message: "first".to_string(),
        })
        .expect("fill channel");

        let result = ctx.emit(AutomatonEvent::LogLine {
            message: "second".to_string(),
        });

        assert!(matches!(result, Err(AutomatonError::EventDelivery(_))));
        assert!(matches!(
            rx.try_recv(),
            Ok(AutomatonEvent::LogLine { message }) if message == "first"
        ));
    }

    #[test]
    fn emit_drops_event_when_receiver_is_closed() {
        let (ctx, rx) = test_context(1);
        drop(rx);

        ctx.emit(AutomatonEvent::LogLine {
            message: "late".to_string(),
        })
        .expect("closed UI event receiver must not fail automaton");
    }
}
