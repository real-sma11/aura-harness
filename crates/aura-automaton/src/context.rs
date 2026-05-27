use tokio::sync::mpsc;
use tracing::warn;

use crate::events::AutomatonEvent;
use crate::state::AutomatonState;
use crate::types::AutomatonId;
use crate::AutomatonError;

pub struct TickContext {
    pub automaton_id: AutomatonId,
    pub state: AutomatonState,
    /// Outbound automaton event channel. Intentionally private — the
    /// **only** way to push protocol events from inside a builtin is
    /// [`Self::emit`], which centralizes the closed-receiver /
    /// channel-full policy (PROTOCOL events log at `error!`, advisory
    /// events log at `warn!`, the channel-full case becomes a typed
    /// `AutomatonError::EventDelivery`). Pre-Phase-6 the field was
    /// `pub`, and the dev-loop / task-run tick paths reached into it
    /// directly to spawn the advisory forwarder — easy to confuse
    /// with the protocol path and easy to bypass the closed-receiver
    /// logging policy. The Phase-6 invariant is: no caller may push
    /// an `AutomatonEvent` here without going through [`Self::emit`];
    /// the one exception is the advisory forwarder spawned by
    /// [`crate::builtins`], which deliberately keeps its own debounce
    /// policy and reaches the sender via the explicitly-named
    /// [`Self::forwarder_sender_clone`] accessor.
    event_tx: mpsc::Sender<AutomatonEvent>,
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
                let kind = event.kind();
                let is_protocol = event.is_protocol();
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
            Err(mpsc::error::TrySendError::Full(event)) => Err(AutomatonError::EventDelivery(
                format!("channel full: {event:?}"),
            )),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub fn cancellation_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.shutdown
    }

    /// Clone the outbound sender for the **advisory** forwarder path
    /// (`crate::builtins::common::forward_event::spawn_agent_event_forwarder`).
    ///
    /// This is the **only** sanctioned way for a builtin to obtain a
    /// raw `Sender<AutomatonEvent>`. Protocol events must always go
    /// through [`Self::emit`]; bypassing this method to push
    /// `Started` / `TaskCompleted` / `Stopped` from a builtin will
    /// silently swallow closed-receiver telemetry that the
    /// `emit`-based logging policy is meant to surface.
    ///
    /// The explicit name keeps the call site self-documenting (a
    /// `grep forwarder_sender_clone` is a tight audit of every
    /// advisory-channel consumer in the workspace).
    pub(crate) fn forwarder_sender_clone(&self) -> mpsc::Sender<AutomatonEvent> {
        self.event_tx.clone()
    }

    /// Borrow the outbound sender for the (small) set of in-crate
    /// helpers that need a `&Sender<AutomatonEvent>` for best-effort
    /// emits (currently
    /// [`crate::builtins::task_refinement::refine_task_description`]).
    /// Same audit-trail rationale as [`Self::forwarder_sender_clone`].
    pub(crate) fn event_sender(&self) -> &mpsc::Sender<AutomatonEvent> {
        &self.event_tx
    }

    /// Workspace-root override as a `String`, with the
    /// empty-string trim already applied. Returns `None` when no
    /// override was installed at automaton start so the caller can
    /// fall back to the project's persisted folder path.
    pub(crate) fn workspace_root_str(&self) -> Option<String> {
        self.workspace_root
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
    }
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
