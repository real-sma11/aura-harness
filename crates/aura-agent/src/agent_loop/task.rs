//! Task shell loop.
//!
//! A *task* is the outermost unit of agent work: it owns the
//! conversation state and (via the enclosing [`crate::session::Session`])
//! the `input_queue` that lets the user steer the agent mid-task.
//! The shell mirrors codex's `tasks::regular::run` shape:
//!
//! ```text
//! loop {
//!     run_turn(...);
//!     if !input_queue.has_pending() { return; }
//! }
//! ```
//!
//! [`run_task`] threads the nesting `run_task → [`super::turn::run_turn`] →
//! [`super::sampling::run_sampling_request`]` and trusts the model's
//! `EndTurn` stop reason as the authoritative end-of-task signal.
//! The `input_queue` is always present today (session-scoped, allocated
//! by [`crate::session::Session`]); the `has_pending()` probe at the
//! end of every turn decides whether to spin another turn or return
//! to the caller. The pre-codex-parity continuation runtime that
//! lived in `session::goal_runtime` is gone — there is no
//! `GoalRuntime::continuation` accumulator; the only persistent
//! cross-turn state is the conversation history on [`super::LoopState`].
//!
//! The task shell owns two safety nets per Rule 4.3:
//!
//! - [`super::AgentLoopConfig::max_turns_per_task`]: hard cap on how
//!   many turns one task can run. Default `50` matches the codex
//!   pattern.
//! - [`super::AgentLoopConfig::max_iterations_per_task`]: hard cap on
//!   the total number of sampling requests across all turns of one
//!   task. Default `500` keeps the existing long-batch workflows
//!   (e.g. multi-`create_task` extraction) inside the envelope
//!   without the silent-cancel regression that the 25-iteration cap
//!   used to cause.
//!
//! Both ceilings surface an [`AgentError::TurnBudgetExceeded`] with
//! structured context so the UI / dashboards can correlate the
//! failure with the task that produced it.

use std::sync::Arc;

use aura_reasoner::{Message, ModelProvider, ToolDefinition};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{field, instrument, Span};
use uuid::Uuid;

use crate::console;
use crate::events::AgentLoopEvent;
use crate::session::goal_runtime::GoalRuntimeEvent;
use crate::session::input_queue::InputQueue;
use crate::session::Session;
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

/// Return the first 8 hex chars of a UUID string (the prefix is
/// universally enough to disambiguate concurrent tasks in a single
/// log file). Used to populate the `task{id=...}` span field without
/// flooding every nested log line with a 36-char UUID.
fn short_id(task_id: &str) -> &str {
    task_id.get(..8).unwrap_or(task_id)
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
#[instrument(
    name = "task",
    skip_all,
    fields(id = field::Empty),
)]
pub(crate) async fn run_task(
    agent: &AgentLoop,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
    event_tx: Option<Sender<AgentLoopEvent>>,
    cancellation_token: Option<CancellationToken>,
    session: Arc<Session>,
) -> Result<AgentLoopResult, AgentError> {
    let task_id = TaskId::new_v4();
    let task_id_str = task_id.to_string();
    Span::current().record("id", field::display(short_id(&task_id_str)));

    let mut state = LoopState::new(&agent.config, messages);
    state.build_baseline = executor.capture_build_baseline().await;

    console::task_start_banner(
        &task_id_str,
        agent.config.max_turns_per_task,
        agent.config.max_iterations_per_task,
    );
    tracing::debug!(
        task_id = %task_id,
        session_id = %session.id,
        max_iterations = agent.config.max_iterations,
        max_turns_per_task = agent.config.max_turns_per_task,
        max_iterations_per_task = agent.config.max_iterations_per_task,
        "Starting agent task"
    );

    // Layer E.4: notify the goal runtime that a new goal has started
    // so subsequent `TurnCompleted` events can be attributed to this
    // task. The streak is session-scoped, so this does NOT reset
    // `ContinuationState::consecutive_no_write` (codex parity).
    let objective = first_user_text(&state.messages).unwrap_or_default();
    if let Err(err) = session
        .goal_runtime
        .handle_event(GoalRuntimeEvent::GoalStarted { task_id, objective })
        .await
    {
        tracing::warn!(error = %err, "goal_runtime::GoalStarted failed; continuing");
    }

    let event_tx_ref = event_tx.as_ref();
    let cancellation_ref = cancellation_token.as_ref();
    let input_queue_arc: Arc<InputQueue> = Arc::clone(&session.input_queue);
    let input_queue_ref: &InputQueue = input_queue_arc.as_ref();

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
            Some(input_queue_ref),
            &session,
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

        // Only spin another turn when the input queue has pending
        // entries. The queue is session-scoped (always present), so
        // the gate is a single `has_pending()` probe — codex parity
        // with `tasks::regular::run`. The pre-codex-parity
        // continuation accumulator on `GoalRuntime` is gone: the
        // only cross-turn state is the conversation history on
        // [`super::LoopState`], so no streak counter needs to be
        // inherited here.
        if input_queue_ref.has_pending() {
            continue;
        }
        break;
    }

    state.result.messages = state.messages;

    for observer in &agent.config.observers {
        observer.on_turn_complete(&state.result).await;
    }

    Ok(state.result)
}

/// Pull the first user-role text content from `messages` for the
/// `GoalStarted` objective field. Returns the empty string when the
/// first user message is missing or carries non-text blocks only.
fn first_user_text(messages: &[Message]) -> Option<String> {
    for msg in messages {
        if matches!(msg.role, aura_reasoner::Role::User) {
            for block in &msg.content {
                if let aura_reasoner::ContentBlock::Text { text } = block {
                    return Some(text.clone());
                }
            }
        }
    }
    None
}
