//! Shared success / failure / cancel transition for one tracked task.
//!
//! Both `dev_loop/tick.rs` and `task_run.rs` re-implemented the same
//! decision tree after `AgentRunner::execute_task_tracked` returned:
//!
//! 1. If the shared cancellation token fired mid-task, treat as a
//!    user-initiated stop: roll the task back to `ready`, emit a
//!    `LogLine`, leave counters untouched.
//! 2. Otherwise on `Ok(TaskExecutionResult)`: transition the task to
//!    `done`, emit `TaskCompleted` + `TokenUsage`, bump the completed
//!    counter (dev-loop only — task-run is single-shot).
//! 3. Otherwise on `Err(AutomatonError)`: transition the task to
//!    `failed`, emit `TaskFailed`, bump the failed counter
//!    (dev-loop only).
//!
//! The dev-loop carries an extra counter-bump and uses the same step
//! for project-shaped loop accounting; the task-run automaton has no
//! per-loop counters. The helper returns a typed [`TaskOutcome`] and
//! lets the caller decide whether to update counters.

use tracing::{info, warn};

use aura_agent::agent_runner::TaskExecutionResult;
use aura_tools::domain_tools::{DomainApi, TaskDescriptor};

use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;

/// Result of [`finalize_task_outcome`] — the high-level branch each
/// caller takes after we've routed the agent-loop result through the
/// domain transition + protocol-event emit.
///
/// Carries forwarded payload (notes / token usage) on the
/// success path so the dev-loop's per-tick counter bookkeeping
/// doesn't have to re-derive them.
/// All three call sites today (`dev_loop` increments per-loop
/// counters, `task_run` just returns `Done`) only need to distinguish
/// the three variants. The token / notes payloads stay on the
/// already-emitted `TaskCompleted` / `TokenUsage` events; future
/// callers that want to consume them can fold the fields back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskOutcome {
    /// The task finished cleanly. The protocol-level
    /// `TaskCompleted` + `TokenUsage` events have already been
    /// emitted via [`crate::TickContext::emit`].
    Success,
    /// The agent loop returned a hard failure. The protocol-level
    /// `TaskFailed` event has already been emitted.
    Failure,
    /// The shared cancellation token fired mid-task. The task was
    /// rolled back to `ready` and a `LogLine` was emitted. Counters
    /// were intentionally not bumped.
    Cancelled,
}

/// Apply the shared post-task transition + event-emit flow.
///
/// `task` is the in-flight task descriptor (used for the id /
/// transition target and the cancellation rollback). `result` is the
/// raw return from `AgentRunner::execute_task_tracked` already wrapped
/// into the typed `AutomatonError`.
///
/// All domain-side transitions are best-effort — they `warn!` on
/// failure rather than propagating, mirroring the pre-Phase-6
/// behaviour in both call sites. The function does propagate
/// `AutomatonError` from any `ctx.emit(...)` call since a closed
/// protocol receiver is a tick-level failure (see
/// `crate::context::TickContext::emit`).
pub(crate) async fn finalize_task_outcome(
    ctx: &mut TickContext,
    domain: &dyn DomainApi,
    task: &TaskDescriptor,
    result: Result<TaskExecutionResult, AutomatonError>,
) -> Result<TaskOutcome, AutomatonError> {
    if ctx.is_cancelled() {
        record_cancelled(ctx, domain, task).await?;
        return Ok(TaskOutcome::Cancelled);
    }

    match result {
        Ok(exec) => {
            record_success(ctx, domain, task, exec).await?;
            Ok(TaskOutcome::Success)
        }
        Err(e) => {
            record_failure(ctx, domain, task, e).await?;
            Ok(TaskOutcome::Failure)
        }
    }
}

async fn record_cancelled(
    ctx: &mut TickContext,
    domain: &dyn DomainApi,
    task: &TaskDescriptor,
) -> Result<(), AutomatonError> {
    info!(
        automaton_id = %ctx.automaton_id,
        task_id = %task.id,
        title = %task.title,
        "Task cancelled by user stop"
    );

    if let Err(e) = domain.transition_task(&task.id, "ready", None).await {
        warn!(
            task_id = %task.id,
            error = %e,
            "Failed to roll cancelled task back to ready"
        );
    }

    ctx.emit(AutomatonEvent::LogLine {
        message: format!("Task {} cancelled by stop request", task.id),
    })?;
    Ok(())
}

async fn record_success(
    ctx: &mut TickContext,
    domain: &dyn DomainApi,
    task: &TaskDescriptor,
    exec: TaskExecutionResult,
) -> Result<(), AutomatonError> {
    if let Err(e) = domain.transition_task(&task.id, "done", None).await {
        warn!(task_id = %task.id, error = %e, "Failed to sync task done status to backend");
    }

    ctx.emit(AutomatonEvent::TaskCompleted {
        task_id: task.id.clone(),
        summary: exec.notes,
    })?;
    ctx.emit(AutomatonEvent::TokenUsage {
        task_id: Some(task.id.clone()),
        input_tokens: exec.input_tokens,
        output_tokens: exec.output_tokens,
    })?;

    info!(task_id = %task.id, title = %task.title, "Task completed successfully");
    Ok(())
}

async fn record_failure(
    ctx: &mut TickContext,
    domain: &dyn DomainApi,
    task: &TaskDescriptor,
    e: AutomatonError,
) -> Result<(), AutomatonError> {
    warn!(task_id = %task.id, error = %e, "Task execution failed");

    if let Err(te) = domain.transition_task(&task.id, "failed", None).await {
        warn!(task_id = %task.id, error = %te, "Failed to sync task failed status to backend");
    }

    ctx.emit(AutomatonEvent::TaskFailed {
        task_id: task.id.clone(),
        reason: e.to_string(),
    })?;
    Ok(())
}
