//! Streaming model calls and event emission.

use std::time::Instant;

use std::time::Duration;

use aura_reasoner::anthropic::exp_backoff_with_jitter;
use aura_reasoner::{
    ModelProvider, ModelRequest, ModelResponse, PartialToolUse, ReasonerError, StreamAccumulator,
    StreamContentType, StreamEvent, StreamEventStream,
};
use chrono::Utc;
use futures_util::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::events::{AgentLoopEvent, DebugEvent};

use super::event_sink;
use super::iteration::LlmCallError;
use super::AgentLoop;

/// Send an event through the channel if present.
///
/// Phase 4: thin re-export of [`event_sink::emit`] so legacy
/// `streaming::emit(...)` call sites keep compiling while the
/// canonical entry point lives next to the unified sink policy.
/// The previous local `try_send + warn` body and the blocking
/// `emit_with_backpressure` helper were removed during the dual-path
/// collapse (see [`event_sink`] module docs for the policy).
pub(super) fn emit(tx: Option<&Sender<AgentLoopEvent>>, event: AgentLoopEvent) {
    event_sink::emit(tx, event);
}

/// Emit an [`AgentLoopEvent::IterationComplete`] event along with the
/// matching [`DebugEvent::Iteration`] frame for the `aura-os` run
/// bundle. `duration_ms` reflects wall-clock time since the start of
/// the current iteration (model call + tool dispatch); `tool_calls` is
/// the number of `ContentBlock::ToolUse` blocks in the response.
pub(super) fn emit_iteration_complete(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    iteration: usize,
    response: &ModelResponse,
    iteration_started_at: Instant,
) {
    emit(
        event_tx,
        AgentLoopEvent::IterationComplete {
            iteration,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
        },
    );

    let tool_calls = response
        .message
        .content
        .iter()
        .filter(|b| matches!(b, aura_reasoner::ContentBlock::ToolUse { .. }))
        .count();

    let duration_ms = u64::try_from(iteration_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let index = u32::try_from(iteration).unwrap_or(u32::MAX);
    let tool_calls = u32::try_from(tool_calls).unwrap_or(u32::MAX);

    emit(
        event_tx,
        AgentLoopEvent::Debug(DebugEvent::Iteration {
            timestamp: Utc::now(),
            index,
            tool_calls,
            duration_ms,
            task_id: None,
        }),
    );
}

/// Emit a [`DebugEvent::LlmCall`] frame. Called at the end of every
/// completed provider call (streaming happy path, non-streaming
/// fallback path, and the compact-and-retry path).
fn emit_debug_llm_call(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    provider_name: &str,
    model_name: &str,
    response: &ModelResponse,
    duration_ms: u64,
) {
    let model = if response.trace.model.is_empty() {
        model_name.to_string()
    } else {
        response.trace.model.clone()
    };
    emit(
        event_tx,
        AgentLoopEvent::Debug(DebugEvent::LlmCall {
            timestamp: Utc::now(),
            provider: provider_name.to_string(),
            model,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            duration_ms,
            task_id: None,
            agent_instance_id: None,
            provider_request_id: response.trace.provider_request_id.clone(),
            message_id: response.trace.message_id.clone(),
        }),
    );
}

/// Map a [`StreamEvent`] to the corresponding [`AgentLoopEvent`] and emit it.
///
/// Phase 4: routes through the unified [`event_sink::emit`] policy
/// (`try_send` + warn/debug) so the buffered streaming path no longer
/// awaits on a saturated channel. The previous
/// `emit_with_backpressure` helper that backpressured the loop on a
/// slow forwarder is gone; per-delta drops are now logged but do not
/// stall sampling.
fn emit_stream_event(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    stream_event: &StreamEvent,
    accumulator: &StreamAccumulator,
) {
    if event_tx.is_none() {
        return;
    }

    match stream_event {
        StreamEvent::TextDelta { text } => {
            emit(event_tx, AgentLoopEvent::TextDelta(text.clone()));
        }
        StreamEvent::ThinkingDelta { thinking } => {
            emit(event_tx, AgentLoopEvent::ThinkingDelta(thinking.clone()));
        }
        StreamEvent::ContentBlockStart {
            content_type: StreamContentType::ToolUse { id, name },
            ..
        } => {
            emit(
                event_tx,
                AgentLoopEvent::ToolStart {
                    id: id.clone(),
                    name: name.clone(),
                },
            );
        }
        StreamEvent::InputJsonDelta { .. } => {
            if let Some(ref tool) = accumulator.current_tool_use {
                emit(
                    event_tx,
                    AgentLoopEvent::ToolInputSnapshot {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        input: tool.input_json.clone(),
                    },
                );
            }
        }
        StreamEvent::Error { message, .. } => {
            emit(
                event_tx,
                AgentLoopEvent::Error {
                    code: "stream_error".to_string(),
                    message: message.clone(),
                    recoverable: true,
                },
            );
        }
        _ => {}
    }
}

/// Outcome of [`drain_remaining_stream`].
///
/// The pump separates "the stream finished cleanly" (the caller now
/// classifies the accumulated response) from "transport blew up" (the
/// caller may want to fall back to non-streaming) and "the user
/// cancelled" (terminal). Returning a typed outcome instead of a
/// `Result<(), Err>` keeps the per-attempt loop callers (the main
/// `complete_with_streaming` orchestrator and the per-tool-call retry
/// path in [`AgentLoop::drive_streaming_once`]) sharing one
/// definition without forcing them to share a single error type.
enum DrainOutcome {
    /// Boxed because [`StreamAccumulator`] is ~320 bytes (vs 96 for
    /// `Transport(ReasonerError)`); the large-variant clippy lint
    /// fires hard for an enum dispatched as a single `match` per
    /// attempt. Indirection here is essentially free — the accumulator
    /// is consumed once via `*acc` at the call site.
    Completed(Box<StreamAccumulator>),
    Cancelled,
    Transport(ReasonerError),
}

/// Pump every event from `stream` into a [`StreamAccumulator`], emitting
/// the corresponding wire deltas as they arrive.
///
/// Honours `cancellation_token` so a Ctrl-C does not have to wait out
/// the full stream. Returns the populated accumulator on clean stream
/// termination; transport errors and cancellation surface as their own
/// [`DrainOutcome`] variants so callers can decide whether to fall
/// back to the non-streaming path or propagate the failure.
async fn drain_remaining_stream(
    mut stream: StreamEventStream,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    cancellation_token: Option<&CancellationToken>,
) -> DrainOutcome {
    let mut accumulator = StreamAccumulator::new();

    loop {
        let next = if let Some(token) = cancellation_token {
            tokio::select! {
                () = token.cancelled() => {
                    return DrainOutcome::Cancelled;
                }
                item = stream.next() => item,
            }
        } else {
            stream.next().await
        };

        match next {
            Some(Ok(event)) => {
                accumulator.process(&event);
                emit_stream_event(event_tx, &event, &accumulator);
            }
            Some(Err(e)) => return DrainOutcome::Transport(e),
            None => return DrainOutcome::Completed(Box::new(accumulator)),
        }
    }
}

/// Run the non-streaming `provider.complete()` path and replay every
/// textual content block as a delta event.
///
/// Used by both fallback branches in [`AgentLoop::complete_with_streaming`]:
/// the transport-error fallback (mid-stream connection blew up before any
/// usable event arrived) and the mid-stream-SSE-error fallback (stream
/// terminated with a retryable `error` frame). Consumers see incremental
/// deltas even though the request was buffered server-side.
async fn complete_and_emit_as_deltas(
    provider: &dyn ModelProvider,
    request: ModelRequest,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    provider_name: &str,
    model_name: &str,
) -> Result<ModelResponse, LlmCallError> {
    let fallback_start = Instant::now();
    let response = provider
        .complete(request)
        .await
        .map_err(|e| LlmCallError::from_reasoner_error(&e))?;
    for block in &response.message.content {
        match block {
            aura_reasoner::ContentBlock::Text { text } => {
                emit(event_tx, AgentLoopEvent::TextDelta(text.clone()));
            }
            aura_reasoner::ContentBlock::Thinking { thinking, .. } => {
                emit(event_tx, AgentLoopEvent::ThinkingDelta(thinking.clone()));
            }
            _ => {}
        }
    }
    let duration_ms = u64::try_from(fallback_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    emit_debug_llm_call(event_tx, provider_name, model_name, &response, duration_ms);
    Ok(response)
}

impl AgentLoop {
    /// Perform a model completion using streaming, emitting events as they arrive.
    ///
    /// Falls back to non-streaming `provider.complete()` only for mid-stream
    /// transport errors. Request-level failures (e.g. 4xx validation errors)
    /// are propagated directly — retrying with a different request format
    /// would not fix them and produces confusing double errors.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) async fn complete_with_streaming(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ModelResponse, LlmCallError> {
        let start = Instant::now();
        let provider_name = provider.name();
        let model_name = request.model.as_ref().to_string();

        let stream = provider
            .complete_streaming(request.clone())
            .await
            .map_err(|e| LlmCallError::from_reasoner_error(&e))?;

        match drain_remaining_stream(stream, event_tx, cancellation_token).await {
            DrainOutcome::Cancelled => Err(LlmCallError::Fatal("Cancelled".to_string())),
            DrainOutcome::Transport(e) => {
                warn!(
                    error = %e,
                    provider = %provider_name,
                    model = %model_name,
                    "Stream transport error; falling back to non-streaming complete()"
                );
                let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let err_str = e.to_string();
                aura_reasoner::console::anthropic_failure_block(
                    aura_reasoner::console::AnthropicFailureView {
                        status_code: Some(200),
                        status_text: "OK",
                        class: "sse_transport",
                        elapsed_ms,
                        request_id: None,
                        retry_after_s: None,
                        body_preview: Some(&err_str),
                        destination: "aura-network",
                    },
                );
                emit(
                    event_tx,
                    AgentLoopEvent::StreamReset {
                        reason: format!("Stream error, retrying without streaming: {e}"),
                    },
                );
                complete_and_emit_as_deltas(provider, request, event_tx, provider_name, &model_name)
                    .await
            }
            DrainOutcome::Completed(accumulator) => {
                self.finalize_after_stream(
                    provider,
                    request,
                    event_tx,
                    cancellation_token,
                    *accumulator,
                    start,
                    provider_name,
                    &model_name,
                )
                .await
            }
        }
    }

    /// Classify the post-pump accumulator into the next action.
    ///
    /// Three buckets:
    /// 1. `Ok(response)` — emit the debug-llm-call frame and return.
    /// 2. `Err(StreamAbortedWithPartial)` — re-issue the streaming
    ///    request via [`Self::retry_streaming_for_partial_tool_use`]
    ///    so the in-flight tool call is preserved across the retry.
    /// 3. `Err(retryable)` — fall back to the non-streaming
    ///    `provider.complete()` path via [`complete_and_emit_as_deltas`].
    /// 4. Anything else — propagate as a fatal LLM call error.
    #[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
    async fn finalize_after_stream(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
        accumulator: StreamAccumulator,
        start: Instant,
        provider_name: &str,
        model_name: &str,
    ) -> Result<ModelResponse, LlmCallError> {
        let latency_ms = start.elapsed().as_millis() as u64;
        match accumulator.into_response(0, latency_ms) {
            Ok(response) => {
                aura_reasoner::console::emit_response_block(&response, latency_ms, 200, "OK");
                emit_debug_llm_call(event_tx, provider_name, model_name, &response, latency_ms);
                Ok(response)
            }
            Err(ReasonerError::StreamAbortedWithPartial {
                reason,
                partial_tool_use,
            }) => {
                // Per-tool-call streaming retry: re-issue a fresh
                // streaming request (up to N attempts) instead of
                // dropping down to the non-streaming fallback, which
                // has no memory of the in-flight tool call and would
                // effectively drop the Write/Edit the model was
                // mid-way through. Matches `fix_4.6-class_failures`
                // plan § `harness-retry-streaming`.
                self.retry_streaming_for_partial_tool_use(
                    provider,
                    request,
                    event_tx,
                    cancellation_token,
                    reason,
                    partial_tool_use,
                )
                .await
            }
            Err(e) if stream_error_is_retryable(&e) => {
                // Upstream emitted a mid-stream SSE `error` event (HTTP 200
                // body, not an HTTP 5xx status). The SSE transport layer
                // has no retry of its own, so without this fallback a
                // single transient provider / proxy blip terminates the
                // whole turn. Re-issuing the call non-streaming routes
                // through `AnthropicProvider::complete`, which has a
                // full retry loop with exponential backoff for 429/529
                // and generic 5xx.
                tracing::warn!(
                    error = %e,
                    "Mid-stream SSE error looks transient; falling back to non-streaming"
                );
                emit(
                    event_tx,
                    AgentLoopEvent::StreamReset {
                        reason: format!("Mid-stream SSE error, retrying without streaming: {e}"),
                    },
                );
                complete_and_emit_as_deltas(provider, request, event_tx, provider_name, model_name)
                    .await
            }
            Err(e) => Err(LlmCallError::from_reasoner_error(&e)),
        }
    }
}

/// Decide whether an error produced by [`StreamAccumulator::into_response`]
/// warrants a fallback to the non-streaming `complete()` path.
///
/// We retry:
///   - mid-stream SSE `error` frames (surfaced as
///     `ReasonerError::Internal("stream terminated with error: ...")`),
///     including the Anthropic `overloaded_error` / `api_error` shapes
///     and the generic `Internal server error` proxies often inject;
///   - context-overflow signals — these are handled upstream but we
///     should not treat them as the "stream died" retry class.
///
/// We deliberately do NOT retry:
///   - `InsufficientCredits` (402): permanent.
///   - `RateLimited` (429/529): the provider has already retried with
///     backoff; the non-streaming path would just hit the same limit.
///   - `Parse` / `Request`: structural issues with the request; re-issuing
///     it verbatim is unlikely to fix anything.
fn stream_error_is_retryable(err: &ReasonerError) -> bool {
    if err.is_context_overflow() {
        return false;
    }
    match err {
        ReasonerError::InsufficientCredits(_)
        | ReasonerError::RateLimited { .. }
        | ReasonerError::Timeout
        | ReasonerError::Parse(_)
        | ReasonerError::Request(_) => false,
        // `Transient` is the structured "retryable upstream 5xx"
        // classification — by definition retryable.
        ReasonerError::Transient { .. } => true,
        ReasonerError::Api { status, .. } => matches!(status, 500 | 502 | 503 | 504),
        ReasonerError::Internal(message) => looks_like_transient_stream_error(message),
        // `StreamAbortedWithPartial` is handled by the dedicated
        // retry loop in `complete_with_streaming` above, not by
        // the legacy non-streaming fallback classifier. Returning
        // `true` here would cause a redundant second fallback
        // path; returning `false` keeps responsibility where it
        // belongs.
        ReasonerError::StreamAbortedWithPartial { .. }
        | ReasonerError::ModelRequestContractViolation(_) => false,
    }
}

fn looks_like_transient_stream_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Match on the canonical prefix set by `StreamAccumulator::into_response`
    // plus the Anthropic / proxy error-type shapes that ride on top of it.
    lower.contains("stream terminated with error")
        || lower.contains("overloaded_error")
        || lower.contains("api_error")
        || lower.contains("internal server error")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
}

// ---------------------------------------------------------------------------
// Per-tool-call streaming retry loop
// ---------------------------------------------------------------------------
//
// Implemented as a free function (not an inherent impl method) to keep
// the `impl AgentLoop { ... complete_with_streaming ... }` block above
// focused on stream orchestration. The caller is a method so
// `Self::retry_streaming_for_partial_tool_use` is still the natural
// call shape, but the function itself lives here at module scope for
// readability.

impl super::AgentLoop {
    /// Re-drive `provider.complete_streaming` after a mid-stream abort
    /// that carried a [`PartialToolUse`]. Emits
    /// [`AgentLoopEvent::ToolCallRetrying`] before every sleep and
    /// [`AgentLoopEvent::ToolCallFailed`] when the retry budget is
    /// exhausted. Returns the first successful `ModelResponse` or the
    /// final error classification.
    async fn retry_streaming_for_partial_tool_use(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
        initial_reason: String,
        initial_partial: Option<PartialToolUse>,
    ) -> Result<ModelResponse, LlmCallError> {
        // Shared with `agent_loop::stream_pump::run_stream_pump` so
        // both retry surfaces honour the same `AURA_LLM_MAX_RETRIES` /
        // `AURA_LLM_BACKOFF_INITIAL_MS` / `AURA_LLM_BACKOFF_CAP_MS`
        // tuning (Phase 3 dedup).
        let (max_retries, backoff_initial_ms, backoff_cap_ms) =
            super::stream_pump::stream_retry_params();
        // Preserve tool identity across retries: once a stream starts
        // and dies before `content_block_start`, subsequent retries may
        // not carry any partial at all — but the UI still benefits from
        // seeing the original tool name if we had one.
        let mut tool_use_id = initial_partial
            .as_ref()
            .map_or_else(|| "<unknown>".to_string(), |p| p.tool_use_id.clone());
        let mut tool_name = initial_partial
            .as_ref()
            .map_or_else(|| "<unknown>".to_string(), |p| p.tool_name.clone());

        let mut last_reason = initial_reason;
        let mut last_err: Option<LlmCallError> = None;

        for attempt in 1..=max_retries {
            let delay = exp_backoff_with_jitter(attempt - 1, backoff_initial_ms, backoff_cap_ms);
            let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);

            // Emit a tracing record per retry so operators see the
            // storm in `aura-node` logs. Without this, a flaky
            // upstream looks like N identical request bodies with no
            // context between them (see `fix-silent-stream-retry-storm`
            // plan: the original UX bug was a ~28 second invisible
            // wait while 7 paid streaming requests fired).
            warn!(
                attempt,
                max_attempts = max_retries,
                delay_ms,
                tool_use_id = %tool_use_id,
                tool_name = %tool_name,
                reason = %last_reason,
                "Per-tool-call streaming retry scheduled after mid-stream abort"
            );

            emit(
                event_tx,
                AgentLoopEvent::ToolCallRetrying {
                    tool_use_id: tool_use_id.clone(),
                    tool_name: tool_name.clone(),
                    attempt,
                    max_attempts: max_retries,
                    delay_ms,
                    reason: last_reason.clone(),
                },
            );

            // Honour cancellation during the backoff so a Ctrl-C
            // doesn't have to wait out a 30s cap.
            if let Some(token) = cancellation_token {
                tokio::select! {
                    () = token.cancelled() => {
                        return Err(LlmCallError::Fatal("Cancelled".to_string()));
                    }
                    () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            match self
                .drive_streaming_once(provider, request.clone(), event_tx, cancellation_token)
                .await
            {
                Ok(response) => return Ok(response),
                Err(DriveStreamingError::AbortedWithPartial {
                    reason,
                    partial_tool_use,
                }) => {
                    if let Some(p) = partial_tool_use {
                        tool_use_id = p.tool_use_id;
                        tool_name = p.tool_name;
                    }
                    last_reason = reason;
                    // fall through: loop continues to next retry
                }
                Err(DriveStreamingError::Other(err)) => {
                    last_err = Some(err);
                    break;
                }
            }
        }

        error!(
            attempts = max_retries,
            tool_use_id = %tool_use_id,
            tool_name = %tool_name,
            reason = %last_reason,
            "Per-tool-call streaming retry budget exhausted; giving up"
        );

        emit(
            event_tx,
            AgentLoopEvent::ToolCallFailed {
                tool_use_id,
                tool_name,
                reason: last_reason.clone(),
            },
        );

        Err(last_err.unwrap_or_else(|| LlmCallError::Fatal(last_reason.clone())))
    }

    /// Drive a single streaming attempt (one `complete_streaming` +
    /// accumulation + classification). Used by the retry loop above so
    /// we can cleanly distinguish "retryable mid-stream abort" from
    /// "other fatal error".
    async fn drive_streaming_once(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ModelResponse, DriveStreamingError> {
        let start = std::time::Instant::now();
        let provider_name = provider.name();
        let model_name = request.model.as_ref().to_string();

        let stream = provider
            .complete_streaming(request)
            .await
            .map_err(|e| DriveStreamingError::Other(LlmCallError::from_reasoner_error(&e)))?;

        let accumulator = match drain_remaining_stream(stream, event_tx, cancellation_token).await {
            DrainOutcome::Completed(acc) => *acc,
            DrainOutcome::Cancelled => {
                return Err(DriveStreamingError::Other(LlmCallError::Fatal(
                    "Cancelled".to_string(),
                )));
            }
            // Transport-level error on the retry attempt. Don't recurse
            // into another non-streaming fallback here — the outer retry
            // loop will decide whether to try again based on
            // classification.
            DrainOutcome::Transport(e) => {
                let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let err_str = e.to_string();
                aura_reasoner::console::anthropic_failure_block(
                    aura_reasoner::console::AnthropicFailureView {
                        status_code: Some(200),
                        status_text: "OK",
                        class: "sse_transport",
                        elapsed_ms,
                        request_id: None,
                        retry_after_s: None,
                        body_preview: Some(&err_str),
                        destination: "aura-network",
                    },
                );
                return Err(DriveStreamingError::Other(
                    LlmCallError::from_reasoner_error(&e),
                ));
            }
        };

        let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        match accumulator.into_response(0, latency_ms) {
            Ok(response) => {
                aura_reasoner::console::emit_response_block(&response, latency_ms, 200, "OK");
                emit_debug_llm_call(event_tx, provider_name, &model_name, &response, latency_ms);
                Ok(response)
            }
            Err(ReasonerError::StreamAbortedWithPartial {
                reason,
                partial_tool_use,
            }) => Err(DriveStreamingError::AbortedWithPartial {
                reason,
                partial_tool_use,
            }),
            Err(e) => Err(DriveStreamingError::Other(
                LlmCallError::from_reasoner_error(&e),
            )),
        }
    }
}

/// Internal classification for a single streaming attempt driven by
/// [`AgentLoop::drive_streaming_once`]. Callers match on this to decide
/// whether the attempt should be retried (AbortedWithPartial) or
/// propagated as-is (Other).
enum DriveStreamingError {
    AbortedWithPartial {
        reason: String,
        partial_tool_use: Option<PartialToolUse>,
    },
    Other(LlmCallError),
}

#[cfg(test)]
mod retry_classifier_tests {
    use super::*;

    #[test]
    fn mid_stream_internal_server_error_is_retryable() {
        let err = ReasonerError::Internal(
            "stream terminated with error (model=claude-sonnet, msg_id=msg_01): Internal server error"
                .to_string(),
        );
        assert!(stream_error_is_retryable(&err));
    }

    #[test]
    fn anthropic_overloaded_error_frame_is_retryable() {
        let err = ReasonerError::Internal(
            "stream terminated with error: overloaded_error: service is overloaded".to_string(),
        );
        assert!(stream_error_is_retryable(&err));
    }

    #[test]
    fn insufficient_credits_is_not_retryable() {
        let err = ReasonerError::InsufficientCredits("402".to_string());
        assert!(!stream_error_is_retryable(&err));
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        let err = ReasonerError::Api {
            status: 400,
            message: "prompt is too long".to_string(),
        };
        assert!(!stream_error_is_retryable(&err));
    }

    #[test]
    fn rate_limited_is_not_retryable_here() {
        let err = ReasonerError::RateLimited {
            message: "429 too many requests".to_string(),
            retry_after: None,
        };
        assert!(!stream_error_is_retryable(&err));
    }

    #[test]
    fn transient_is_retryable() {
        let err = ReasonerError::Transient {
            status: 502,
            message: "bad gateway".to_string(),
            retry_after: None,
        };
        assert!(stream_error_is_retryable(&err));
    }

    #[test]
    fn api_5xx_is_retryable() {
        assert!(stream_error_is_retryable(&ReasonerError::Api {
            status: 500,
            message: "upstream".to_string(),
        }));
        assert!(stream_error_is_retryable(&ReasonerError::Api {
            status: 502,
            message: "bad gateway".to_string(),
        }));
        assert!(!stream_error_is_retryable(&ReasonerError::Api {
            status: 400,
            message: "bad request".to_string(),
        }));
    }
}
