//! Single sampling request driver (Layer E.1).
//!
//! A *sampling request* is one round-trip with the model provider: one
//! pre-call compaction pass, one [`ModelProvider::complete`] (streaming
//! or buffered), one response-accumulation pass, one
//! `dispatch_stop_reason` step that may execute tool calls in a single
//! batch (Phase 3 parallel-tools stays batched in E.1), and one
//! `iteration_complete` event.
//!
//! Sampling is the innermost loop level in the codex topology
//! ([codex-rs/core/src/session/turn.rs:1747 analog](
//! https://github.com/.../codex-rs/core/src/session/turn.rs)). The
//! [`turn::run_turn`] driver calls [`run_sampling_request`] repeatedly
//! until the model signals `EndTurn` *and* no [`turn::run_turn_stop_hooks`]
//! injection requests another follow-up.
//!
//! Invariants:
//!
//! - Cancellation observed before `call_model` short-circuits to a
//!   `needs_follow_up = false`, `broke_for_error = true` outcome so the
//!   turn loop unwinds without paying for one more model call.
//! - On `LlmCallError`, the error is applied to [`AgentLoopResult`] via
//!   [`iteration::LlmCallError::apply`] and `broke_for_error = true`
//!   instructs the turn loop to break (preserves the pre-E.1 behavior
//!   where a fatal model error terminated the loop immediately).
//! - `state.result.iterations` is incremented inside this function (the
//!   counter is "completed sampling requests", matching pre-E.1 shape).

use std::time::Instant;

use aura_reasoner::{ModelProvider, StopReason, ToolDefinition};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument};

use crate::events::AgentLoopEvent;
use crate::types::AgentToolExecutor;

use super::stream_pump::{StreamPumpOutcome, run_stream_pump};
use super::{AgentLoop, LoopState, context, is_cancelled, iteration, streaming};

/// Outcome of a single sampling request inside a turn.
///
/// The fields mirror codex's `SamplingRequestResult` shape but are
/// `pub(crate)` per Rule 3.1 — they never cross the crate boundary.
pub(crate) struct SamplingRequestResult {
    /// Whether the turn loop should continue to another sampling
    /// request based purely on the model's signal (`ToolUse` or
    /// `MaxTokens` with pending tool calls). Combined with the
    /// `injected_continuation` outcome from
    /// [`super::turn::run_turn_stop_hooks`] to produce the final
    /// `needs_follow_up` decision in the turn loop.
    pub(crate) needs_follow_up: bool,
    /// Why the model emitted its terminal event this sampling
    /// request. Carried in the sampling result so E.3's stream-level
    /// driver can distinguish a clean `EndTurn` from a `MaxTokens`
    /// truncation when deciding whether to drain in-flight tools
    /// without firing another sampling. E.1's turn loop folds the
    /// outcome into the `needs_follow_up` bit and currently doesn't
    /// inspect this field; the field is preserved so E.3 / E.4 can
    /// consume it without re-plumbing the return shape.
    #[allow(dead_code)] // Consumed by E.3 (stream driver) / E.4 (GoalRuntime).
    pub(crate) stop_reason: StopReason,
    /// `true` when the sampling failed in a way that the turn loop
    /// must observe (fatal model error, cancellation). In this case
    /// the loop must break and not run stop hooks — the result has
    /// already been mutated with `llm_error` / `cancelled`.
    pub(crate) broke_for_error: bool,
}

/// Drive one sampling request to completion.
///
/// Mirrors the per-iteration body of the pre-E.1 `AgentLoop::run_inner`
/// loop: compaction, request build, model call (with overflow retry),
/// response accumulation, iteration-complete event, and
/// stop-reason dispatch (which may execute tool calls in a single
/// batch). Returns a [`SamplingRequestResult`] that lets the enclosing
/// turn loop decide whether to continue with another sampling request.
///
/// The argument list intentionally holds every dependency the body
/// touches (provider, executor, tools, event sink, cancellation token,
/// mutable `LoopState`, iteration counter) so the function stays
/// free-standing and trivially callable from `turn::run_turn`. E.3 (the
/// stream-driver phase) collapses these into a `SamplingContext`
/// struct; until then we suppress `clippy::too_many_arguments` rather
/// than introduce a one-shot builder type that would be thrown away
/// almost immediately.
#[allow(clippy::too_many_arguments)] // E.3 collapses into SamplingContext.
#[instrument(
    name = "sampling",
    skip_all,
    fields(iter = iteration),
)]
pub(crate) async fn run_sampling_request(
    agent: &AgentLoop,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    tools: &[ToolDefinition],
    event_tx: Option<&Sender<AgentLoopEvent>>,
    cancellation_token: Option<&CancellationToken>,
    state: &mut LoopState,
    iteration: usize,
) -> SamplingRequestResult {
    if is_cancelled(cancellation_token) {
        debug!("Cancellation requested before sampling, stopping loop");
        return SamplingRequestResult {
            needs_follow_up: false,
            stop_reason: StopReason::EndTurn,
            broke_for_error: true,
        };
    }

    state.begin_iteration(&agent.config, iteration);
    let iteration_started_at = Instant::now();

    match context::compact_if_needed(&agent.config, state, tools) {
        context::CompactionOutcome::NeedsSummary(input) => {
            agent
                .apply_summary_compaction(
                    provider,
                    tools,
                    event_tx,
                    cancellation_token,
                    state,
                    input,
                )
                .await;
        }
        context::CompactionOutcome::Applied(tier) => {
            debug!(?tier, "local compaction applied before model call");
        }
        context::CompactionOutcome::None => {}
    }

    let request = match state.build_request(&agent.config, tools, iteration) {
        Ok(r) => r,
        Err(e) => {
            iteration::LlmCallError::Fatal(e.to_string()).apply(&mut state.result, event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                stop_reason: StopReason::EndTurn,
                broke_for_error: true,
            };
        }
    };

    // Layer E.3: the streaming pump path consumes
    // `provider.complete_response_stream(…)` incrementally and overlaps
    // tool execution at `OutputItemDone` boundaries. Gated behind
    // `use_stream_pump` so chat callers that need per-delta event
    // emission stay on the legacy `call_model` + `dispatch_stop_reason`
    // path until E.4 wires per-delta emission into the pump.
    if agent.config.use_stream_pump {
        return run_sampling_request_streaming(
            agent,
            provider,
            executor,
            request,
            event_tx,
            cancellation_token,
            state,
            iteration,
            iteration_started_at,
        )
        .await;
    }

    let response = match agent
        .call_model(provider, request, event_tx, cancellation_token)
        .await
    {
        Ok(r) => r,
        Err(iteration::LlmCallError::PromptTooLong(msg)) => {
            match agent
                .retry_after_context_overflow(
                    provider,
                    tools,
                    iteration,
                    event_tx,
                    cancellation_token,
                    state,
                    msg,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    e.apply(&mut state.result, event_tx);
                    return SamplingRequestResult {
                        needs_follow_up: false,
                        stop_reason: StopReason::EndTurn,
                        broke_for_error: true,
                    };
                }
            }
        }
        Err(e) => {
            e.apply(&mut state.result, event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                stop_reason: StopReason::EndTurn,
                broke_for_error: true,
            };
        }
    };

    if let Some(input) = iteration::accumulate_response(&agent.config, state, &response) {
        agent
            .apply_summary_compaction(provider, tools, event_tx, cancellation_token, state, input)
            .await;
    }
    state.result.iterations = iteration + 1;
    streaming::emit_iteration_complete(event_tx, iteration, &response, iteration_started_at);

    // Stop fired during or right after streaming finished — don't
    // dispatch a fresh tool batch (which would race for minutes against
    // the cancellation observed at the top of the next sampling).
    // Cheap "cancelled before any tool dispatch" bail-out so the loop
    // terminates immediately instead of paying for one more
    // (potentially long) tool round-trip.
    if is_cancelled(cancellation_token) {
        debug!("Cancellation observed after model call; skipping tool dispatch");
        return SamplingRequestResult {
            needs_follow_up: false,
            stop_reason: response.stop_reason,
            broke_for_error: true,
        };
    }

    // `dispatch_stop_reason` returns `true` when the loop should break.
    // The codex topology inverts this into `needs_follow_up` so the
    // outer turn loop can fold the model's signal together with stop
    // hooks / queued input (E.2/E.4) into a single termination
    // predicate.
    let dispatch_says_break = agent
        .dispatch_stop_reason(&response, executor, event_tx, state)
        .await;

    SamplingRequestResult {
        needs_follow_up: !dispatch_says_break,
        stop_reason: response.stop_reason,
        broke_for_error: false,
    }
}

/// Streaming pump entry point for [`run_sampling_request`] (Layer E.3).
///
/// Replaces the buffered `call_model` + `dispatch_stop_reason` block
/// with the [`stream_pump`] pipeline: opens a
/// [`aura_reasoner::ResponseEventStream`], spawns tool futures into a
/// [`futures_util::stream::FuturesOrdered`] as `OutputItemDone(tool_use)`
/// events arrive, and drains the FIFO after the model signals
/// `Completed`. Tool results from the pump are then folded back into
/// the existing per-sampling accumulation / iteration-complete /
/// stop-reason dispatch helpers so the rest of the loop sees the same
/// shape it does in the legacy path.
///
/// The split with [`run_sampling_request`] keeps the legacy buffered
/// callers untouched while this commit lands; once E.4 wires per-delta
/// event emission through the pump, the legacy branch can collapse.
#[allow(clippy::too_many_arguments)] // Mirrors run_sampling_request shape.
async fn run_sampling_request_streaming(
    agent: &AgentLoop,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    request: aura_reasoner::ModelRequest,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    cancellation_token: Option<&CancellationToken>,
    state: &mut LoopState,
    iteration: usize,
    iteration_started_at: Instant,
) -> SamplingRequestResult {
    let outcome = run_stream_pump(
        &agent.config,
        provider,
        executor,
        request,
        cancellation_token,
        // The streaming pump opts to drain the input queue once per
        // `OutputItemDone(tool_use)` so user steering inserted
        // mid-batch becomes visible to the very next sampling
        // request without losing the in-flight tool drain. The
        // sampling driver itself does not own the input queue
        // (that's the `task::run_task` scope), so we pass `None`
        // here and let the task shell drive per-turn drains as
        // before — the pump's per-event drain stays no-op for
        // sampling callers that don't plumb a queue through.
        None,
        // Layer E.4: forward the event channel through to the pump
        // so per-`OutputItemDone` block emits the same
        // `TextDelta` / `ThinkingDelta` / `ToolStart` /
        // `ToolInputSnapshot` events the buffered streaming path
        // already produced. This is the parity gap that kept the
        // pump opt-in pre-E.4.
        event_tx,
        state,
    )
    .await;

    let (response, tool_results) = match outcome {
        StreamPumpOutcome::Completed {
            response,
            tool_results,
        } => (response, tool_results),
        StreamPumpOutcome::Cancelled => {
            debug!("stream pump observed cancellation; bailing the sampling request");
            return SamplingRequestResult {
                needs_follow_up: false,
                stop_reason: StopReason::EndTurn,
                broke_for_error: true,
            };
        }
        StreamPumpOutcome::Error(err) => {
            let llm_err = match err {
                crate::AgentError::Reason(inner) => {
                    iteration::LlmCallError::from_reasoner_error(&inner)
                }
                other => iteration::LlmCallError::Fatal(other.to_string()),
            };
            llm_err.apply(&mut state.result, event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                stop_reason: StopReason::EndTurn,
                broke_for_error: true,
            };
        }
        StreamPumpOutcome::AbortedWithPartial { .. } => {
            iteration::LlmCallError::Fatal(
                "stream pump returned an unretried partial tool-use abort".to_string(),
            )
            .apply(&mut state.result, event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                stop_reason: StopReason::EndTurn,
                broke_for_error: true,
            };
        }
    };

    if let Some(input) = iteration::accumulate_response(&agent.config, state, &response) {
        agent
            .apply_summary_compaction(provider, &[], event_tx, cancellation_token, state, input)
            .await;
    }
    state.result.iterations = iteration + 1;
    streaming::emit_iteration_complete(event_tx, iteration, &response, iteration_started_at);

    if is_cancelled(cancellation_token) {
        debug!("Cancellation observed after stream pump; skipping tool dispatch");
        return SamplingRequestResult {
            needs_follow_up: false,
            stop_reason: response.stop_reason,
            broke_for_error: true,
        };
    }

    let dispatch_says_break = super::stream_pump::dispatch_streamed_response(
        agent,
        executor,
        &response,
        tool_results,
        event_tx,
        state,
    )
    .await;

    SamplingRequestResult {
        needs_follow_up: !dispatch_says_break,
        stop_reason: response.stop_reason,
        broke_for_error: false,
    }
}
