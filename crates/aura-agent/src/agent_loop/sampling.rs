//! Single sampling request driver (Phase 4 unified, Phase 7 pruned).
//!
//! A *sampling request* is one round-trip with the model provider:
//! one pre-call compaction pass, one
//! [`super::transport::ModelTransport::sample`] call (the pump
//! transport returned by [`super::transport::active_transport`]),
//! one response-accumulation pass, one `iteration_complete` event,
//! and one unified [`super::tool_pipeline::dispatch`] step that may
//! execute (or pass through) one batch of tool calls.
//!
//! Phase 4 collapsed the previously-duplicated
//! `run_sampling_request` / `run_sampling_request_streaming` pair
//! behind a single function. Phase 7 then deleted the legacy
//! buffered transport (parity proven, no caller flipped off the
//! pump) so the single tail now runs:
//! cancellation check → `accumulate_response` →
//! `emit_iteration_complete` → `tool_pipeline::dispatch`.
//!
//! Invariants:
//!
//! - Cancellation observed before the transport call short-circuits
//!   to a `needs_follow_up = false`, `broke_for_error = true`
//!   outcome so the turn loop unwinds without paying for one more
//!   model call.
//! - On [`super::iteration::LlmCallError`], the error is applied to
//!   [`crate::types::AgentLoopResult`] via
//!   [`super::iteration::LlmCallError::apply`] and
//!   `broke_for_error = true` instructs the turn loop to break
//!   (preserves the pre-Phase-4 behavior where a fatal model error
//!   terminated the loop immediately).
//! - `state.result.iterations` is incremented inside this function
//!   (the counter is "completed sampling requests", matching the
//!   pre-Phase-4 shape).
//! - Mid-tool cancellation inside the pump folds `[CANCELLED]`
//!   tool_results into a `Streamed` outcome with `stop_loop = true`
//!   markers so the Anthropic `tool_use ↔ tool_result` adjacency
//!   contract stays intact through `dispatch`. The post-sample
//!   cancellation probe therefore bails ONLY when the batch is
//!   empty (no in-flight tools to repair).

use std::time::Instant;

use tracing::{debug, instrument};

use super::cx::TurnCtx;
use super::tool_pipeline::{self, ToolBatch, ToolEffectCtx};
use super::transport::{self, SamplingCtx, TransportOutcome};
use super::{context, is_cancelled, iteration, streaming, LoopState};

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
    /// `true` when the sampling failed in a way that the turn loop
    /// must observe (fatal model error, cancellation). In this case
    /// the loop must break and not run stop hooks — the result has
    /// already been mutated with `llm_error` / `cancelled`.
    pub(crate) broke_for_error: bool,
    /// `true` when the model response carried something the user can
    /// see — non-empty assistant text or at least one `tool_use`
    /// block. `false` for an empty or thinking-only response. The
    /// turn loop accumulates this so it can surface a clear
    /// "turn ended without action" signal (and optionally re-prompt)
    /// instead of letting a no-op turn look like a silent hang.
    pub(crate) produced_visible_output: bool,
    /// `true` when leaked tool-call markup was scrubbed from the
    /// assistant text this sampling request (the model wrote
    /// `<invoke>` / `[tool_use ...>` markup as prose instead of a
    /// native `tool_use` block, so no tool actually ran). The turn
    /// loop reads the *last* sampling request's value to recover the
    /// "turn ends after one message because the tool call was eaten"
    /// failure by re-prompting once instead of ending the turn.
    pub(crate) scrubbed_tool_markup: bool,
}

/// Whether a model response carries user-visible output: a non-empty
/// assistant text block or any `tool_use` block. A response that is
/// only an (extended) thinking block — or entirely empty — returns
/// `false`. Used by the turn loop's never-silent / no-op handling.
fn response_has_visible_output(response: &aura_model_reasoner::ModelResponse) -> bool {
    use aura_model_reasoner::ContentBlock;
    response.message.content.iter().any(|block| match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        ContentBlock::ToolUse { .. } => true,
        _ => false,
    })
}

/// Drive one sampling request to completion (Phase 4 unified body).
///
/// Mirrors the per-iteration body of the pre-Phase-3 `run_inner`
/// loop with the dual buffered/pump split collapsed behind
/// [`super::transport::ModelTransport`]. Returns a
/// [`SamplingRequestResult`] that lets the enclosing turn loop decide
/// whether to continue with another sampling request.
///
/// Phase 8: the entire per-run + per-turn borrow surface (provider,
/// executor, event sink, cancellation token, agent, session, tools,
/// input_queue) collapses into the single [`TurnCtx`] borrow; the
/// still-distinct per-sample argument (`iteration` plus the mutable
/// `state`) stays explicit so callers can step the iteration counter
/// without unpacking the bundle.
#[instrument(
    name = "sampling",
    skip_all,
    fields(iter = iteration),
)]
pub(crate) async fn run_sampling_request(
    ctx: &TurnCtx<'_>,
    state: &mut LoopState,
    iteration: usize,
) -> SamplingRequestResult {
    let run = ctx.run;
    let tools = ctx.tools;
    if is_cancelled(run.cancellation_token) {
        debug!("Cancellation requested before sampling, stopping loop");
        return SamplingRequestResult {
            needs_follow_up: false,
            broke_for_error: true,
            produced_visible_output: false,
            scrubbed_tool_markup: false,
        };
    }

    state.begin_iteration(&run.agent.config, iteration);
    let iteration_started_at = Instant::now();

    match context::compact_if_needed(&run.agent.config, state, tools, iteration) {
        context::CompactionOutcome::NeedsSummary(input) => {
            run.agent
                .apply_summary_compaction(
                    run.provider,
                    tools,
                    run.event_tx,
                    run.cancellation_token,
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

    let request = match state.build_request(&run.agent.config, tools, iteration) {
        Ok(r) => r,
        Err(e) => {
            iteration::LlmCallError::Fatal(e.to_string()).apply(&mut state.result, run.event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                broke_for_error: true,
                produced_visible_output: false,
                scrubbed_tool_markup: false,
            };
        }
    };

    // Phase 4 + 7: route the model call through the pump transport
    // returned by `active_transport`. The transport produces a
    // [`TransportOutcome`] that flattens to a
    // `(ModelResponse, ToolBatch)` pair so the post-sample tail
    // stays uniform.
    let transport_impl = transport::active_transport();
    let sampling_ctx = SamplingCtx {
        agent: run.agent,
        provider: run.provider,
        executor: run.executor,
        tools,
        event_tx: run.event_tx,
        cancellation_token: run.cancellation_token,
        input_queue: ctx.input_queue,
        state,
        request,
        iteration,
    };
    let outcome = match transport_impl.sample(sampling_ctx).await {
        Ok(o) => o,
        Err(e) => {
            e.apply(&mut state.result, run.event_tx);
            return SamplingRequestResult {
                needs_follow_up: false,
                broke_for_error: true,
                produced_visible_output: false,
                scrubbed_tool_markup: false,
            };
        }
    };

    let (response, batch) = match outcome {
        TransportOutcome::Streamed(streamed) => {
            let crate::agent_loop::transport::StreamedOutcome {
                response,
                pre_executed,
            } = *streamed;
            (response, ToolBatch::PreExecuted(pre_executed))
        }
        TransportOutcome::Cancelled => {
            debug!("transport observed cancellation; bailing the sampling request");
            return SamplingRequestResult {
                needs_follow_up: false,
                broke_for_error: true,
                produced_visible_output: false,
                scrubbed_tool_markup: false,
            };
        }
    };

    let produced_visible_output = response_has_visible_output(&response);

    let scrubbed_tool_markup =
        iteration::accumulate_response(&run.agent.config, state, &response, iteration);
    state.result.iterations = iteration + 1;
    streaming::emit_iteration_complete(run.event_tx, iteration, &response, iteration_started_at);

    // Stop fired during or right after sampling: bail before
    // dispatching a fresh batch UNLESS the pump pre-executed a
    // batch carrying `[CANCELLED]` synthetic tool_results — those
    // must still flow through `dispatch` so the Anthropic
    // `tool_use ↔ tool_result` adjacency contract stays intact.
    // When the batch is empty (no in-flight tools, e.g.
    // cancellation arrived before any `OutputItemDone(tool_use)`),
    // we keep the fast bail-out path.
    if is_cancelled(run.cancellation_token) && batch.is_empty() {
        debug!("Cancellation observed after model call; skipping tool dispatch");
        return SamplingRequestResult {
            needs_follow_up: false,
            broke_for_error: true,
            produced_visible_output,
            scrubbed_tool_markup,
        };
    }

    let tool_ctx = ToolEffectCtx {
        executor: run.executor,
        event_tx: run.event_tx,
        cancellation_token: run.cancellation_token,
    };
    let dispatch_says_break =
        tool_pipeline::dispatch(run.agent, state, &response, batch, tool_ctx).await;

    SamplingRequestResult {
        needs_follow_up: !dispatch_says_break,
        broke_for_error: false,
        produced_visible_output,
        scrubbed_tool_markup,
    }
}
