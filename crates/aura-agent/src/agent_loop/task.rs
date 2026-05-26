//! Task shell loop (Layer E.1).
//!
//! A *task* is the outermost unit of agent work: it owns the
//! conversation state, the build baseline, and (once E.2 lands) the
//! `input_queue` that lets the user steer the agent mid-task. Codex's
//! task shell at [codex-rs/core/src/tasks/regular.rs:73 analog](
//! https://github.com/.../codex-rs/core/src/tasks/regular.rs) drives
//! the pattern:
//!
//! ```text
//! loop {
//!     run_turn(...);
//!     if !input_queue.has_pending() { return; }
//! }
//! ```
//!
//! E.1 wired the nesting (`run_task` → [`super::turn::run_turn`] →
//! [`super::sampling::run_sampling_request`]) with the
//! `input_queue.has_pending()` probe stubbed out to `false`. E.2 lifts
//! the stub: when an `input_queue` is supplied, the task shell loops
//! until the queue is empty AND the active turn terminates cleanly.
//! When no queue is supplied (`input_queue == None`), the task shell
//! runs exactly one turn — preserving the pre-E.1 behaviour where
//! one `AgentLoop::run_inner` call drove the whole conversation.
//!
//! The task shell owns two safety nets per Rule 4.3:
//!
//! - [`AgentLoopConfig::max_turns_per_task`]: hard cap on how many
//!   turns one task can run. Default `50` matches the codex pattern.
//! - [`AgentLoopConfig::max_iterations_per_task`]: hard cap on the
//!   total number of sampling requests across all turns of one task.
//!   Default `500` keeps the existing long-batch workflows
//!   (e.g. multi-`create_task` extraction) inside the envelope
//!   without the silent-cancel regression that the 25-iteration cap
//!   used to cause.
//!
//! Both ceilings surface an
//! [`AgentError::TurnBudgetExceeded`] with structured context so the
//! UI / dashboards can correlate the failure with the task that
//! produced it.

use std::sync::Arc;

use aura_reasoner::{Message, ModelProvider, ToolDefinition};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

use crate::events::AgentLoopEvent;
use crate::session::input_queue::InputQueue;
use crate::types::{AgentLoopResult, AgentToolExecutor};
use crate::AgentError;

use super::turn::{run_turn, TurnOutcome};
use super::{AgentLoop, LoopState};

/// Newtype wrapper around a `Uuid` identifying one in-flight task.
///
/// Generated at task start by [`run_task`] and threaded through the
/// turn loop so that
/// [`AgentError::TurnBudgetExceeded`](crate::AgentError::TurnBudgetExceeded)
/// can attribute the budget overrun to a specific task (Rule 4.3,
/// Rule 5.1). E.2 will extend this into a `SessionId` / `TaskId`
/// pair when the session struct lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Mint a fresh v4 task identifier.
    ///
    /// Used by [`run_task`] when callers do not supply one (the
    /// pre-E.1 entry points do not have access to the wider session
    /// scope where the id would otherwise be allocated).
    #[must_use]
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new_v4()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Drive one task to completion.
///
/// E.1 wired the codex-shaped nesting (task → turn → sampling). E.2
/// wires the optional [`InputQueue`] into the outer loop so that
/// mid-task user inputs cause the task shell to spin another turn
/// after the active turn drains the queue. When no queue is supplied
/// (`input_queue == None`), the loop falls through to the
/// `terminated_cleanly` short-circuit and the task runs at most one
/// turn — preserving the E.1 single-turn-per-task semantic for
/// callers that opt out of mid-task steering.
///
/// `iteration_offset` accumulates by the per-tool-batch count
/// (`turn_outcome.sampling_count`) across turns, since input-queue
/// restarts always happen at turn boundaries — never mid-sampling.
/// This keeps `state.result.iterations` monotonically increasing and
/// also makes the `max_iterations_per_task` cap count completed
/// sampling requests, which is what the per-task budget semantics
/// document.
///
/// Returns the populated [`AgentLoopResult`] regardless of whether the
/// task terminated cleanly or short-circuited on a fatal model error.
/// Only the per-task hard ceilings (`max_turns_per_task`,
/// `max_iterations_per_task`) surface as `Err(AgentError::…)`; every
/// other failure mode is materialised on `state.result` (so the
/// pre-E.1 caller contract — "`run` always returns `Ok` with errors
/// folded into the result" — survives).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_task(
    agent: &AgentLoop,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
    event_tx: Option<Sender<AgentLoopEvent>>,
    cancellation_token: Option<CancellationToken>,
    input_queue: Option<Arc<InputQueue>>,
) -> Result<AgentLoopResult, AgentError> {
    let task_id = TaskId::new_v4();
    let mut state = LoopState::new(&agent.config, messages);
    state.build_baseline = executor.capture_build_baseline().await;
    info!(
        task_id = %task_id,
        max_iterations = agent.config.max_iterations,
        max_turns_per_task = agent.config.max_turns_per_task,
        max_iterations_per_task = agent.config.max_iterations_per_task,
        has_input_queue = input_queue.is_some(),
        "Starting agent task"
    );

    let event_tx_ref = event_tx.as_ref();
    let cancellation_ref = cancellation_token.as_ref();
    let input_queue_ref = input_queue.as_deref();

    // E.2: turn_index / iteration_offset accumulate across turns so
    // the `max_turns_per_task` / `max_iterations_per_task` caps trip
    // on a genuine runaway. iteration_offset is bumped by the per-
    // turn `sampling_count` (= per-tool-batch count) because every
    // sampling request is one model round-trip and the input-queue
    // restart only happens at turn boundaries — never mid-sampling.
    let mut turn_index: u32 = 0;
    let mut iteration_offset: u32 = 0;

    loop {
        // Hard ceiling: surface a typed error per Rule 4.3 instead of
        // silently terminating. Trip before the next `run_turn` call
        // so we never half-execute another turn past the cap.
        if turn_index >= agent.config.max_turns_per_task {
            return Err(AgentError::TurnBudgetExceeded {
                task_id,
                turn_index,
            });
        }
        if iteration_offset >= agent.config.max_iterations_per_task {
            return Err(AgentError::TurnBudgetExceeded {
                task_id,
                turn_index,
            });
        }

        let turn_outcome: TurnOutcome = run_turn(
            agent,
            provider,
            executor,
            &tools,
            event_tx_ref,
            cancellation_ref,
            &mut state,
            task_id,
            turn_index,
            iteration_offset,
            input_queue_ref,
        )
        .await?;

        // Accumulate per-turn sampling count into the per-task
        // counters BEFORE any early-break paths so the post-turn
        // `state.result.iterations` math stays consistent even on
        // error exits.
        iteration_offset = iteration_offset.saturating_add(turn_outcome.sampling_count);
        turn_index = turn_index.saturating_add(1);

        if turn_outcome.broke_for_error {
            break;
        }
        if !turn_outcome.terminated_cleanly {
            break;
        }

        // E.2: only spin another turn when the input queue has
        // pending entries. Without a queue (or with an empty one),
        // the task is done as soon as the active turn breaks cleanly.
        match input_queue_ref {
            Some(queue) if queue.has_pending() => continue,
            _ => break,
        }
    }

    state.result.messages = state.messages;

    for observer in &agent.config.observers {
        observer.on_turn_complete(&state.result).await;
    }

    Ok(state.result)
}
