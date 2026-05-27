//! Phase 4 keystone — single [`ModelTransport`] trait collapsing the
//! buffered ([`super::streaming::complete_with_streaming`]) and
//! streaming pump ([`super::stream_pump::run_stream_pump`]) sampling
//! paths behind one async surface.
//!
//! # Why
//!
//! Pre-Phase-4 the agent loop carried two near-duplicate sampling
//! entry points in [`super::sampling`]: `run_sampling_request`
//! (buffered) and `run_sampling_request_streaming` (pump). Roughly
//! 80% of the body was shared — cancellation probe,
//! `accumulate_response`, `emit_iteration_complete`,
//! `dispatch_stop_reason` — but the two duplicated retry, error
//! mapping, and tool-batch handoff. The duplication was the
//! single biggest source of dual-path parity bugs.
//!
//! Phase 4 folds the model-sampling step into a trait so the
//! enclosing sampling driver runs the
//! cancellation / accumulate / iteration_complete / dispatch tail
//! exactly once regardless of transport. The
//! [`TransportOutcome`] variants carry the only thing the two
//! paths legitimately produce differently (a pre-executed tool batch
//! for the pump path) and downstream `process_tool_results` consumes
//! either via [`super::tool_pipeline::ToolBatch::Live`] (buffered) or
//! [`super::tool_pipeline::ToolBatch::PreExecuted`] (pump).
//!
//! # SamplingCtx vs ToolEffectCtx
//!
//! [`SamplingCtx`] bundles the per-sample arguments — agent,
//! provider, executor, tools, event channel, cancellation token,
//! input queue, mutable loop state, and the freshly-built request.
//! Both transports take it by value (move) so the `&mut LoopState`
//! borrow inside is single-use per sample; the unified
//! `run_sampling_request` rebuilds a fresh ctx if it ever needs a
//! second pass (overflow retry inside the buffered transport
//! handles its own rebuild internally).
//!
//! [`super::tool_pipeline::ToolEffectCtx`] is a *separate*, much
//! smaller bundle threaded through `process_tool_results` (executor,
//! event_tx, cancellation_token). The two contexts intentionally do
//! not share a struct: sample-time needs the request and provider,
//! tool-effect-time needs neither, and packing them together would
//! force the post-sample dispatch path to carry dead fields.
//!
//! # Cancellation contract
//!
//! Pre-Phase-4 each transport had its own bailout: the buffered
//! path returned `LlmCallError::Fatal("Cancelled")`; the pump
//! returned [`super::stream_pump::StreamPumpOutcome::Cancelled`].
//! [`TransportOutcome::Cancelled`] now unifies these into a single
//! "no llm_error, broke_for_error = true" signal so the sampling
//! driver can short-circuit without applying a synthetic
//! `llm_error` string to the result.
//!
//! Mid-tool cancellation inside the pump still folds `[CANCELLED]`
//! tool_results into a `Streamed` outcome with the synthetic
//! `stop_loop = true` markers (see
//! `super::stream_pump::driver::cancelled_outcome`) so the
//! Anthropic `tool_use ↔ tool_result` adjacency contract stays
//! intact through `process_tool_results`. Returning `Cancelled`
//! here is reserved for the "no tool_use blocks emitted yet" arms.

use aura_reasoner::{ModelProvider, ModelRequest, ModelResponse, ToolDefinition};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::events::AgentLoopEvent;
use crate::session::input_queue::InputQueue;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

use super::iteration::LlmCallError;
use super::stream_pump::{run_stream_pump, StreamPumpOutcome};
use super::{AgentLoop, LoopState};

/// Bundle of borrowed per-sample dependencies handed to
/// [`ModelTransport::sample`] each turn.
///
/// All fields except [`Self::request`] are borrowed; the request is
/// owned because it is built once per sample (see
/// [`super::state::LoopState::build_request`]) and the transport
/// consumes it (the pump uses `.clone()` internally for retries, so
/// it is `Clone`).
///
/// `&mut LoopState` is stored so the pump path can update
/// [`super::cache::ToolResultCache`] / `state.messages` / the
/// repeated-read tracker mid-stream; the buffered path takes it but
/// does not need to mutate `state` from inside `sample` — the
/// outer `run_sampling_request` body owns post-response mutations.
pub(crate) struct SamplingCtx<'a> {
    pub(crate) agent: &'a AgentLoop,
    pub(crate) provider: &'a dyn ModelProvider,
    pub(crate) executor: &'a dyn AgentToolExecutor,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) event_tx: Option<&'a Sender<AgentLoopEvent>>,
    pub(crate) cancellation_token: Option<&'a CancellationToken>,
    pub(crate) input_queue: Option<&'a InputQueue>,
    pub(crate) state: &'a mut LoopState,
    pub(crate) request: ModelRequest,
    pub(crate) iteration: usize,
}

/// One sampling round-trip outcome.
///
/// `Buffered` carries the model response only: tool execution
/// happens later through [`super::tool_pipeline::process_tool_results`]
/// against a fresh `ToolBatch::Live` batch.
///
/// `Streamed` carries the response plus the FIFO-ordered
/// pre-executed tool batch the pump already ran inside the streaming
/// driver. The same `process_tool_results` entry point consumes it
/// as `ToolBatch::PreExecuted` — `track_tool_effects` / `auto-build`
/// / message-push still run uniformly on both paths.
///
/// `Cancelled` is the "no llm_error, just break" short-circuit. It
/// fires when the cancellation token observed during sampling did
/// NOT have any in-flight tool_use blocks to repair (otherwise the
/// pump path folds `[CANCELLED]` tool_results into `Streamed`).
pub(crate) enum TransportOutcome {
    Buffered(ModelResponse),
    Streamed {
        response: ModelResponse,
        pre_executed: Vec<(ToolCallInfo, ToolCallResult)>,
    },
    Cancelled,
}

/// The keystone trait: one method, one outcome enum, two
/// implementations ([`BufferedTransport`] / [`PumpTransport`]).
///
/// Implementations live in this module; the trait stays
/// `pub(crate)` per Rule 3.1 — no external consumer plugs in their
/// own transport today (the surface would also need to model
/// retry/backoff for that to be useful).
#[async_trait::async_trait]
pub(crate) trait ModelTransport: Send + Sync {
    /// Drive one sampling request to terminal completion.
    ///
    /// Returns [`TransportOutcome::Buffered`] for the legacy
    /// `complete_with_streaming` path, [`TransportOutcome::Streamed`]
    /// for the pump path (with the pre-executed tool batch), or
    /// [`TransportOutcome::Cancelled`] when the cancellation token
    /// fired before any tool_use blocks were emitted.
    ///
    /// # Errors
    ///
    /// Returns the structured [`LlmCallError`] for fatal model
    /// errors (rate-limit, prompt-too-long, insufficient credits,
    /// transport blowups). The buffered transport handles its own
    /// `PromptTooLong` retry ladder inside `sample` so the outer
    /// driver never needs to know whether retry succeeded.
    async fn sample(&self, ctx: SamplingCtx<'_>) -> Result<TransportOutcome, LlmCallError>;
}

/// Buffered transport: wraps the legacy
/// [`AgentLoop::complete_with_streaming`] path.
///
/// Internally drives `provider.complete_streaming(...)` (or
/// `provider.complete(...)` when `event_tx` is `None`), accumulates
/// the full response, and returns it in [`TransportOutcome::Buffered`].
/// The Phase 7 plan tracks deleting this implementation once pump
/// parity is fully proven in production; until then it remains the
/// fallback path (toggle via [`super::AgentLoopConfig::use_stream_pump`]).
pub(crate) struct BufferedTransport;

#[async_trait::async_trait]
impl ModelTransport for BufferedTransport {
    async fn sample(&self, ctx: SamplingCtx<'_>) -> Result<TransportOutcome, LlmCallError> {
        let SamplingCtx {
            agent,
            provider,
            tools,
            event_tx,
            cancellation_token,
            state,
            request,
            iteration,
            ..
        } = ctx;

        let response = match agent
            .call_model(provider, request, event_tx, cancellation_token)
            .await
        {
            Ok(r) => r,
            Err(LlmCallError::PromptTooLong(msg)) => {
                // Two-tier overflow-recovery ladder lives on
                // [`AgentLoop::retry_after_context_overflow`]. It
                // rebuilds the request after aggressive / micro
                // compaction and re-enters `call_model`, so the
                // recovery path stays inside the buffered transport
                // (mixing it with the pump would not work — the
                // overflow detection only surfaces from
                // `provider.complete` / `complete_streaming`).
                agent
                    .retry_after_context_overflow(
                        provider,
                        tools,
                        iteration,
                        event_tx,
                        cancellation_token,
                        state,
                        msg,
                    )
                    .await?
            }
            Err(e) => return Err(e),
        };

        Ok(TransportOutcome::Buffered(response))
    }
}

/// Streaming pump transport: wraps the
/// [`run_stream_pump`] entry point.
///
/// Drives `provider.complete_response_stream(...)` with per-event
/// timeout, overlaps tool execution at `OutputItemDone` boundaries
/// via [`futures_util::stream::FuturesOrdered`], and returns the
/// pre-executed tool batch in [`TransportOutcome::Streamed`].
/// Mid-tool cancellation folds `[CANCELLED]` tool_results into the
/// `pre_executed` vec (see `driver::cancelled_outcome`) so the
/// downstream `process_tool_results` step closes the Anthropic
/// adjacency contract before the loop breaks.
pub(crate) struct PumpTransport;

#[async_trait::async_trait]
impl ModelTransport for PumpTransport {
    async fn sample(&self, ctx: SamplingCtx<'_>) -> Result<TransportOutcome, LlmCallError> {
        let SamplingCtx {
            agent,
            provider,
            executor,
            event_tx,
            cancellation_token,
            input_queue,
            state,
            request,
            ..
        } = ctx;

        let outcome = run_stream_pump(
            &agent.config,
            provider,
            executor,
            request,
            cancellation_token,
            input_queue,
            event_tx,
            state,
        )
        .await;

        match outcome {
            StreamPumpOutcome::Completed {
                response,
                tool_results,
            } => Ok(TransportOutcome::Streamed {
                response,
                pre_executed: tool_results,
            }),
            StreamPumpOutcome::Cancelled => Ok(TransportOutcome::Cancelled),
            StreamPumpOutcome::Error(err) => {
                let llm_err = match err {
                    crate::AgentError::Reason(inner) => LlmCallError::from_reasoner_error(&inner),
                    other => LlmCallError::Fatal(other.to_string()),
                };
                Err(llm_err)
            }
            StreamPumpOutcome::AbortedWithPartial { .. } => Err(LlmCallError::Fatal(
                "stream pump returned an unretried partial tool-use abort".to_string(),
            )),
        }
    }
}

/// Resolve the active transport for `config`.
///
/// Returns a reference to a static instance so the sampling driver
/// can hand `&dyn ModelTransport` to `sample` without per-turn
/// allocation. The toggle reads [`super::AgentLoopConfig::use_stream_pump`]
/// directly so flipping the flag at runtime (e.g. by test fixtures
/// that disable the pump for parity checks) takes effect on the
/// very next sample.
pub(crate) fn select_transport(config: &super::AgentLoopConfig) -> &'static dyn ModelTransport {
    if config.use_stream_pump {
        &PumpTransport
    } else {
        &BufferedTransport
    }
}
