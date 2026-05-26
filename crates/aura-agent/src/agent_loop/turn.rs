//! Turn driver (codex-parity simplified).
//!
//! A *turn* is the unit of agent work between the model "starting to
//! talk" and "going quiet without a follow-up signal". Codex's turn
//! loop runs a sequence of sampling requests until
//!
//! ```text
//! needs_follow_up = model_says_continue
//!     || has_pending_input
//! ```
//!
//! evaluates to `false`. Once the model emits `EndTurn` (or a
//! non-tool-use stop reason) and the input queue is drained, the
//! turn unwinds and the task shell decides whether to spin a new
//! turn.
//!
//! Invariants:
//!
//! - The turn loop terminates as soon as `needs_follow_up == false`.
//! - Cancellation / fatal model errors short-circuit the loop without
//!   running stop hooks (the result is already finalised).
//! - The queue drain at the top of every iteration uses a
//!   `biased; select!` so cancellation observed during the drain wins
//!   over any newly-queued user input. The message-append step that
//!   follows the drain is atomic with respect to that cancellation —
//!   there is no half-written message state.

use aura_reasoner::{Message, ModelProvider, ToolDefinition};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::console;
use crate::events::AgentLoopEvent;
use crate::session::input_queue::InputQueue;
use crate::session::{Session, UserInput};
use crate::types::AgentToolExecutor;
use crate::{helpers, AgentError};

use super::sampling::{run_sampling_request, SamplingRequestResult};
use super::{context, AgentLoop, LoopState, TaskId};

/// Result of a single turn.
///
/// Fields capture just enough context to let the outer task shell
/// (`task::run_task`) decide whether to keep running turns.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnOutcome {
    /// `true` when the turn loop broke because the model signalled
    /// stop *and* no pending input requested a follow-up. The task
    /// shell uses this together with the queue's `has_pending` flag
    /// to decide whether to spin another turn.
    pub(crate) terminated_cleanly: bool,
    /// `true` when the turn loop broke because a stop hook signalled
    /// `should_break` (budget exhaustion) or a fatal model error /
    /// cancellation was observed. The task shell reads this to skip
    /// any "restart on pending input" behavior.
    pub(crate) broke_for_error: bool,
    /// Number of sampling requests completed inside this turn. Used
    /// by the outer task shell to accumulate the
    /// `max_iterations_per_task` counter and for debug logging.
    pub(crate) sampling_count: u32,
}

/// Outcome of [`run_turn_stop_hooks`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StopHookOutcome {
    /// `true` when the loop must terminate (budget exhausted). The
    /// turn loop breaks and the task shell observes
    /// `TurnOutcome::broke_for_error`.
    pub(crate) should_break: bool,
}

/// Drive one turn to completion.
///
/// The loop body is the codex-shaped polarity flip: each iteration
/// drains the optional [`InputQueue`] (with cancellation precedence),
/// runs one sampling request, then asks `needs_follow_up?` (model
/// signal OR pending user input). When the answer is `false` the
/// turn terminates; otherwise the loop continues.
///
/// `iteration_offset` is the running sampling-request counter shared
/// with the task shell so that `state.result.iterations` keeps a
/// monotonically-increasing total across turns.
///
/// `input_queue` is the optional mid-task user steering buffer. When
/// `Some`, the queue is drained at the top of every sampling
/// iteration (the drained inputs become user-role context for the
/// next model call) AND its `has_pending()` flag participates in
/// the `needs_follow_up` predicate so the loop keeps looping while
/// the user is still feeding it work. When `None`, behaviour
/// collapses to one drain-free sampling loop until the model
/// signals stop.
#[allow(clippy::too_many_arguments)]
#[instrument(
    name = "turn",
    skip_all,
    fields(idx = turn_index, iter_offset = iteration_offset),
)]
pub(crate) async fn run_turn(
    agent: &AgentLoop,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    tools: &[ToolDefinition],
    event_tx: Option<&Sender<AgentLoopEvent>>,
    cancellation_token: Option<&CancellationToken>,
    state: &mut LoopState,
    task_id: TaskId,
    turn_index: u32,
    iteration_offset: u32,
    input_queue: Option<&InputQueue>,
    session: &Session,
) -> Result<TurnOutcome, AgentError> {
    let mut sampling_count: u32 = 0;
    let mut terminated_cleanly = false;
    let mut broke_for_error = false;

    loop {
        let iteration =
            usize::try_from(iteration_offset.saturating_add(sampling_count)).unwrap_or(usize::MAX);

        // Drain pending user input BEFORE the budget check so the
        // cancel branch of the biased select! unwinds without
        // counting against the per-task ceilings. Cancellation
        // observed here is the in-band `UserInput::Cancel` path.
        if let Some(queue) = input_queue {
            match drain_pending_input(queue, cancellation_token).await {
                DrainOutcome::Drained(inputs) => {
                    if !inputs.is_empty() {
                        apply_user_inputs_to_messages(&mut state.messages, inputs);
                    }
                }
                DrainOutcome::Cancelled => {
                    broke_for_error = true;
                    break;
                }
            }
        }

        // Hard ceiling: max_iterations is the global cap (default
        // `usize::MAX`). Trip it BEFORE the next sampling so we never
        // pay for one more model call past the budget.
        if agent.config.max_iterations != usize::MAX && iteration >= agent.config.max_iterations {
            return Err(AgentError::TurnBudgetExceeded {
                task_id,
                turn_index,
            });
        }

        // Visual separator at the top of each sampling iteration so
        // operators can scan a single log file and immediately see
        // where one round-trip ends and the next begins.
        console::sampling_boundary(&task_id.to_string(), turn_index, iteration);

        let sampling_result: SamplingRequestResult = run_sampling_request(
            agent,
            provider,
            executor,
            tools,
            event_tx,
            cancellation_token,
            state,
            iteration,
        )
        .await;

        sampling_count = sampling_count.saturating_add(1);

        if sampling_result.broke_for_error {
            broke_for_error = true;
            break;
        }

        // Codex shape: `needs_follow_up` defaults to "continue". When
        // the model signals follow-up (ToolUse / MaxTokens with
        // pending), the post-sampling stop hooks run for budget /
        // checkpoint side-effects only.
        if sampling_result.needs_follow_up {
            let stop_outcome =
                run_turn_stop_hooks(&agent.config, event_tx, state, iteration, task_id, session)
                    .await?;
            if stop_outcome.should_break {
                broke_for_error = true;
                break;
            }
            continue;
        }

        // The model signalled stop, but pending user input keeps the
        // turn loop alive — the next iteration's drain will pull the
        // queued context into `state.messages` and feed it to a
        // fresh sampling request.
        if input_queue.is_some_and(InputQueue::has_pending) {
            continue;
        }

        terminated_cleanly = true;
        break;
    }

    Ok(TurnOutcome {
        terminated_cleanly,
        broke_for_error,
        sampling_count,
    })
}

/// Outcome of a single biased-select drain at the top of the turn
/// loop. Separates "queue had inputs to apply" from "external (or
/// in-band) cancellation fired during the drain" so the caller can
/// branch on each path without a flag-passing chain.
enum DrainOutcome {
    Drained(Vec<UserInput>),
    Cancelled,
}

/// Atomically drain `queue` with cancellation precedence (Rule 6.3).
///
/// The `select!` is `biased;` so a cancellation that fired before
/// (or alongside) the drain always wins; only a clean drain reaches
/// the caller's message-append step. Atomicity of the message
/// append is preserved because the drain consumes the buffer in one
/// step before any `state.messages` mutation runs — there is no
/// half-write window even if cancellation fires immediately after
/// this returns. When no cancellation token is supplied, the drain
/// runs unconditionally (no select! at all).
async fn drain_pending_input(
    queue: &InputQueue,
    cancellation_token: Option<&CancellationToken>,
) -> DrainOutcome {
    match cancellation_token {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => DrainOutcome::Cancelled,
                inputs = queue.drain() => DrainOutcome::Drained(inputs),
            }
        }
        None => DrainOutcome::Drained(queue.drain().await),
    }
}

/// Apply a FIFO-ordered batch of [`UserInput`] entries to the
/// conversation message history.
///
/// - [`UserInput::Message`] entries are appended via
///   [`helpers::append_warning`] so the trailing-user-message merge
///   rule (required for Anthropic `tool_use`/`tool_result` adjacency)
///   is preserved.
/// - [`UserInput::Steer`] entries are wrapped in a
///   `<harness_steer>` envelope so the model can distinguish a
///   user-typed message from a harness-on-behalf directive (the
///   wrapper is unindented free text — no XML escaping needed for
///   the model surface).
/// - [`UserInput::Cancel`] entries are dropped because the
///   cancellation token was already fired by
///   [`InputQueue::push`]; the in-band variant is only enqueued for
///   the tracing paper trail and that paper trail has already been
///   served by the queue itself.
///
/// Exposed as `pub(super)` so the stream pump
/// ([`super::sampling`]) can call it after a per-`OutputItemDone`
/// drain without duplicating the merge / envelope logic.
pub(super) fn apply_user_inputs_to_messages(messages: &mut Vec<Message>, inputs: Vec<UserInput>) {
    for input in inputs {
        match input {
            UserInput::Message(text) => {
                helpers::append_warning(messages, &text);
            }
            UserInput::Steer { instruction } => {
                let body = format!("<harness_steer>\n{instruction}\n</harness_steer>");
                helpers::append_warning(messages, &body);
            }
            UserInput::Cancel => {}
        }
    }
}

/// Run the post-sampling stop hooks for a single turn iteration.
///
/// Codex parity: this no longer delegates to a continuation runtime.
/// Responsibilities reduced to:
///
/// 1. Emit the first-write checkpoint warning at most once per run.
/// 2. Emit budget warnings.
/// 3. Trip the credit-budget stop.
pub(crate) async fn run_turn_stop_hooks(
    config: &super::AgentLoopConfig,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut LoopState,
    iteration: usize,
    _task_id: TaskId,
    _session: &Session,
) -> Result<StopHookOutcome, AgentError> {
    let mut outcome = StopHookOutcome::default();

    context::emit_checkpoint_if_needed(event_tx, state);

    context::check_budget_warnings(config, event_tx, state, iteration);
    if context::should_stop_for_budget(config, state, iteration) {
        state.result.timed_out = true;
        outcome.should_break = true;
    }

    Ok(outcome)
}
