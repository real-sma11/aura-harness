//! Placeholder goal-runtime surface.
//!
//! The pre-codex-parity version of this module owned the in-loop
//! continuation runtime: it tracked consecutive no-write turns,
//! buffered failed-write attempts, detected "circling" read patterns,
//! and routed continuation prompts back through the [`InputQueue`].
//!
//! That entire machinery is gone — the dev loop now mirrors codex's
//! [`tasks::regular`] shape (`loop { run_turn; if !has_pending_input
//! { return; } }`) and trusts the model's `EndTurn` signal as the
//! authoritative termination boundary. Forcing continuation prompts
//! after `EndTurn` was the source of the doom-loop documented in
//! `docs/codex-comparison.md` §11 (R-TOOL-1).
//!
//! What remains here is the minimum surface needed by the task /
//! session glue:
//!
//! - [`GoalRuntime`] stores the `session_id` so callers can attribute
//!   future side-channels (telemetry, prompt summaries) to the right
//!   session without re-plumbing a separate type.
//! - [`GoalRuntimeEvent`] is the typed event the task shell still
//!   emits at the boundaries the parent observer cares about
//!   (`GoalStarted` / `SessionIdle` / `MaybeContinueIfIdle`). All
//!   handlers are no-ops; the enum exists so callers do not need to
//!   change every emission site when the runtime is reintroduced.
//! - [`GoalRuntime::handle_event`] always returns `Ok(())`.
//!
//! If you find yourself adding more state here, that is a strong
//! signal to revisit `docs/codex-comparison.md` first — most of the
//! behaviour the old runtime owned should live *outside* the loop
//! body, on the session-idle path, exactly the way codex models it.

use crate::AgentError;

use super::SessionId;

// Re-export `TaskId` here so external callers and tests do not have to
// reach into `crate::agent_loop::task` through a sealed module path.
pub(crate) use crate::agent_loop::TaskId;

/// Lifecycle event consumed by the [`GoalRuntime`].
///
/// All variants are inert today — the placeholder runtime accepts the
/// event and returns `Ok(())` without mutating state. They are kept
/// so the task shell and any future session-idle observer can emit
/// the same signal without re-plumbing the call sites.
#[derive(Debug, Clone)]
pub(crate) enum GoalRuntimeEvent {
    /// Emitted by the task shell once at the start of a new task.
    GoalStarted {
        #[allow(dead_code)]
        task_id: TaskId,
        #[allow(dead_code)]
        objective: String,
    },
    /// Emitted when the session has fully drained (no in-flight task,
    /// no pending input).
    #[allow(dead_code)]
    SessionIdle,
    /// Probe emitted by future schedulers asking the runtime whether
    /// to spin a follow-up turn while the session sits idle.
    #[allow(dead_code)]
    MaybeContinueIfIdle,
}

/// Session-scoped placeholder for the future goal-continuation runtime.
///
/// Today it carries only the `session_id` so callers can attribute
/// events to the right session. The pre-codex-parity fields
/// (`active_goal`, `continuation`, `max_continuation_turns`) and
/// their backing `Mutex`es are intentionally absent — the loop owns
/// no continuation state.
pub(crate) struct GoalRuntime {
    session_id: SessionId,
}

impl GoalRuntime {
    /// Build a fresh runtime for a session.
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    /// Session this runtime belongs to.
    #[allow(dead_code)]
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Accept a lifecycle event. Always succeeds in the placeholder.
    pub(crate) async fn handle_event(&self, _event: GoalRuntimeEvent) -> Result<(), AgentError> {
        Ok(())
    }
}
