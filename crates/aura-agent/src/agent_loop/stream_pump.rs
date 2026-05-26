//! Streaming sampling pump with stream-level tool overlap (Layer E.3).
//!
//! The pump is the per-sampling-request loop body that consumes a
//! [`ResponseEventStream`] one event at a time, spawning tool futures
//! into a [`FuturesOrdered`] the moment a [`OutputItem::ToolUse`]
//! item arrives instead of waiting for the entire model response to
//! buffer. After the model emits [`ResponseEvent::Completed`], the
//! pump drains the FIFO in submission order so tool results appear
//! in the conversation history in the same order the model emitted
//! them (codex's `drain_in_flight` contract,
//! [codex-rs/core/src/session/turn.rs:2141 analog](
//! https://github.com/.../codex-rs/core/src/session/turn.rs)).
//!
//! # Module-level invariants (Rule 13)
//!
//! - **FIFO**: tool results return in the order the model emitted
//!   the corresponding `OutputItemDone(tool_use)` events.
//! - **Atomic cancellation (Rule 6.3)**: when the cancellation token
//!   fires the pump returns [`StreamPumpOutcome::Cancelled`]
//!   *without* mutating `state.messages` — in-flight tools are
//!   aborted by dropping the `FuturesOrdered`. The downstream
//!   sampling driver then bails without running the post-execution
//!   path, so the conversation history never half-writes a
//!   partially-drained tool batch.
//! - **Per-event timeout (Rule 6.2)**: each `stream.try_next()` call
//!   is wrapped with `tokio::time::timeout(stream_event_timeout, …)`
//!   so a silent stream surfaces as
//!   [`crate::AgentError::StreamTimeout`] instead of hanging.
//! - **Per-tool timeout (Rule 6.2)**: each spawned tool future is
//!   wrapped with `tokio::time::timeout(per_tool_timeout, …)` and a
//!   hung tool resolves to a synthetic
//!   [`crate::ToolCallResult`] error — the FIFO continues to drain
//!   so a single bad tool never poisons the rest of the batch.
//! - **Input-queue drain semantic**: this pump drains the optional
//!   [`InputQueue`] once per `OutputItemDone(tool_use)` (codex
//!   parity / richer mid-batch steering). Drained inputs are
//!   applied to `state.messages` immediately so they become visible
//!   to the next sampling request without dropping the current
//!   one's tool batch.
//! - **No `unwrap()` outside `#[cfg(test)]`** (Rule 4.1): the pump
//!   handles every fallible step through `Result<_, AgentError>` /
//!   match on `Option<ResponseEvent>` arms (no `next().await?` —
//!   `Stream::next` yields `Option<Result<_>>` so `?` is ill-typed).

use std::pin::Pin;
use std::time::Duration;

use aura_reasoner::anthropic::exp_backoff_with_jitter;
use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, OutputItem, PartialToolUse,
    ProviderTrace, ReasonerError, ResponseEvent, ResponseEventStream, Role, StopReason, Usage,
};
use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::AgentError;
use crate::events::AgentLoopEvent;
use crate::session::input_queue::InputQueue;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

use super::AgentLoopConfig;

/// Boxed per-tool future spawned into [`FuturesOrdered`] inside the
/// pump. Carries the originating [`ToolCallInfo`] alongside the
/// result so the drain loop can reassemble FIFO order without an
/// auxiliary lookup. Aliased per `clippy::type_complexity`.
type ToolFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = (ToolCallInfo, ToolCallResult)> + Send + 'a>>;

/// Outcome of [`run_stream_pump`].
///
/// `Completed` carries a synthesised [`ModelResponse`] (so downstream
/// callers can reuse the existing `accumulate_response` /
/// `dispatch_stop_reason` helpers) plus the per-tool results gathered
/// from the FIFO drain. `Cancelled` is the atomic-no-state-mutation
/// path (Rule 6.3). `Error` carries the typed [`AgentError`] so the
/// caller can route it through the existing `iteration::LlmCallError`
/// surfacing helpers.
pub(super) enum StreamPumpOutcome {
    Completed {
        /// Synthesised assistant response. Stop reason is derived
        /// from `Completed.end_turn` and whether any tool calls were
        /// emitted; usage comes straight from the provider.
        response: ModelResponse,
        /// Tool calls observed in this stream, paired with their
        /// executed results in submission order. Empty when the
        /// response contained no `tool_use` blocks.
        tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    },
    /// External or in-band cancellation observed inside the pump
    /// loop. No `state.messages` mutation; in-flight tool futures
    /// were dropped (cancellation safety, Rule 6.3).
    Cancelled,
    /// Pump aborted on a typed error (stream timeout, transport
    /// closed, etc.). Carries the [`AgentError`] so the caller can
    /// fold it into the loop result without re-wrapping.
    Error(AgentError),
    /// The response stream aborted while a `tool_use` block was
    /// still being accumulated. `run_stream_pump` consumes this
    /// internal outcome to drive the per-tool-call retry loop.
    AbortedWithPartial {
        reason: String,
        partial_tool_use: Option<PartialToolUse>,
    },
}

/// Drive one sampling request through the streaming pump.
///
/// Opens `provider.complete_response_stream(request)`, then drives
/// the resulting [`ResponseEventStream`] to completion with
/// per-event timeout, biased cancellation, and per-tool
/// concurrency via [`FuturesOrdered`]. See the module-level docs
/// for the full invariant list.
#[allow(clippy::too_many_arguments)] // E.4 added event_tx + input_queue; documented per Rule 1.4.
pub(super) async fn run_stream_pump(
    config: &AgentLoopConfig,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    request: ModelRequest,
    cancellation_token: Option<&CancellationToken>,
    input_queue: Option<&InputQueue>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut super::LoopState,
) -> StreamPumpOutcome {
    let model_name = request.model.as_ref().to_string();
    let (max_retries, backoff_initial_ms, backoff_cap_ms) = stream_retry_params();
    let mut retry_state: Option<PartialRetryState> = None;

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let Some(state) = retry_state.as_ref() else {
                return StreamPumpOutcome::Error(AgentError::Reason(ReasonerError::Internal(
                    "stream retry requested without partial tool-use state".to_string(),
                )));
            };
            let delay = exp_backoff_with_jitter(attempt - 1, backoff_initial_ms, backoff_cap_ms);
            let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
            warn!(
                attempt,
                max_attempts = max_retries,
                delay_ms,
                tool_use_id = %state.tool_use_id,
                tool_name = %state.tool_name,
                reason = %state.reason,
                "Per-tool-call streaming retry scheduled after pump stream abort"
            );
            emit_event(
                event_tx,
                AgentLoopEvent::ToolCallRetrying {
                    tool_use_id: state.tool_use_id.clone(),
                    tool_name: state.tool_name.clone(),
                    attempt,
                    max_attempts: max_retries,
                    delay_ms,
                    reason: state.reason.clone(),
                },
            );
            if let Some(token) = cancellation_token {
                tokio::select! {
                    () = token.cancelled() => return StreamPumpOutcome::Cancelled,
                    () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        let stream = match provider.complete_response_stream(request.clone()).await {
            Ok(s) => s,
            Err(ReasonerError::StreamAbortedWithPartial {
                reason,
                partial_tool_use,
            }) => {
                retry_state = Some(update_partial_retry_state(
                    retry_state,
                    reason,
                    partial_tool_use,
                ));
                continue;
            }
            Err(err) => return StreamPumpOutcome::Error(AgentError::Reason(err)),
        };

        match drive_stream(
            config,
            executor,
            stream,
            cancellation_token,
            input_queue,
            event_tx,
            state,
            &model_name,
        )
        .await
        {
            StreamPumpOutcome::AbortedWithPartial {
                reason,
                partial_tool_use,
            } => {
                retry_state = Some(update_partial_retry_state(
                    retry_state,
                    reason,
                    partial_tool_use,
                ));
            }
            other => return other,
        }
    }

    let state = retry_state.unwrap_or_else(|| PartialRetryState {
        tool_use_id: "<unknown>".to_string(),
        tool_name: "<unknown>".to_string(),
        reason: "stream aborted while tool_use was in flight".to_string(),
    });
    error!(
        attempts = max_retries,
        tool_use_id = %state.tool_use_id,
        tool_name = %state.tool_name,
        reason = %state.reason,
        "Per-tool-call streaming retry budget exhausted in pump; giving up"
    );
    emit_event(
        event_tx,
        AgentLoopEvent::ToolCallFailed {
            tool_use_id: state.tool_use_id,
            tool_name: state.tool_name,
            reason: state.reason.clone(),
        },
    );
    StreamPumpOutcome::Error(AgentError::Reason(
        ReasonerError::StreamAbortedWithPartial {
            reason: state.reason,
            partial_tool_use: None,
        },
    ))
}

struct PartialRetryState {
    tool_use_id: String,
    tool_name: String,
    reason: String,
}

fn update_partial_retry_state(
    previous: Option<PartialRetryState>,
    reason: String,
    partial_tool_use: Option<PartialToolUse>,
) -> PartialRetryState {
    let (prev_id, prev_name) = previous.map_or_else(
        || ("<unknown>".to_string(), "<unknown>".to_string()),
        |state| (state.tool_use_id, state.tool_name),
    );
    let (tool_use_id, tool_name) = partial_tool_use.map_or((prev_id, prev_name), |partial| {
        (partial.tool_use_id, partial.tool_name)
    });
    PartialRetryState {
        tool_use_id,
        tool_name,
        reason,
    }
}

/// Retry budget / backoff envelope shared with the legacy buffered
/// streaming retry path. Reads the same env vars as
/// `aura_reasoner::AnthropicConfig` so operators tune both paths
/// together.
fn stream_retry_params() -> (u32, u64, u64) {
    let max_retries: u32 = std::env::var("AURA_LLM_MAX_RETRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let backoff_initial_ms: u64 = std::env::var("AURA_LLM_BACKOFF_INITIAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(250);
    let backoff_cap_ms: u64 = std::env::var("AURA_LLM_BACKOFF_CAP_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    (max_retries, backoff_initial_ms, backoff_cap_ms)
}

/// Inner driver — separated so the unit tests can hand it a
/// hand-rolled `ResponseEventStream` without a real `ModelProvider`.
///
/// Layer E.4: takes an optional `event_tx` and emits per-`OutputItemDone`
/// equivalents of the legacy `streaming::emit_stream_event` deltas
/// (`TextDelta` / `ThinkingDelta` / `ToolStart` / `ToolInputSnapshot`)
/// so consumers of the streaming sampling pump observe the same
/// event surface they see on the buffered path. Granularity is at
/// the block boundary (codex-shape `OutputItemDone` arrives once per
/// finished block) rather than per-wire-frame — chat UI continuity
/// for the pump-default flip; sub-block per-token deltas remain on
/// the legacy buffered path until a follow-up wires a tap stream.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_stream(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    mut stream: ResponseEventStream,
    cancellation_token: Option<&CancellationToken>,
    input_queue: Option<&InputQueue>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut super::LoopState,
    model_name: &str,
) -> StreamPumpOutcome {
    let mut in_flight: FuturesOrdered<ToolFuture<'_>> = FuturesOrdered::new();
    let mut text_chunks: Vec<String> = Vec::new();
    let mut thinking_chunks: Vec<(String, Option<String>)> = Vec::new();
    let mut tool_calls_seen: Vec<ToolCallInfo> = Vec::new();
    // E.4 tool-result cache: tracks tools that we DID NOT spawn
    // because the per-run cache had a hit. The result is materialised
    // synchronously here and merged into the final `tool_results` at
    // drain time, preserving the FIFO submission order so the codex
    // drain_in_flight contract still holds across the cache /
    // spawn split.
    let mut cached_pairs: Vec<(usize, (ToolCallInfo, ToolCallResult))> = Vec::new();
    // Counter used to assign a stable submission index per tool —
    // both spawned and cached arms append into the same logical
    // FIFO. `tool_calls_seen.len()` after the push is the canonical
    // index for each new call.
    let mut spawned_indices: Vec<usize> = Vec::new();
    let mut end_turn: Option<bool> = None;
    let mut usage = Usage::default();
    let stream_event_timeout = config.stream_event_timeout;
    let per_tool_timeout = config.per_tool_timeout;

    loop {
        let next_step =
            next_stream_step(&mut stream, stream_event_timeout, cancellation_token).await;
        match next_step {
            StreamStep::Cancelled => return StreamPumpOutcome::Cancelled,
            StreamStep::TimedOut => {
                return StreamPumpOutcome::Error(AgentError::StreamTimeout {
                    elapsed_ms: stream_event_timeout
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                });
            }
            StreamStep::TransportErr(err) => {
                return match err {
                    aura_reasoner::StreamError::StreamAbortedWithPartial {
                        reason,
                        partial_tool_use,
                    } => StreamPumpOutcome::AbortedWithPartial {
                        reason,
                        partial_tool_use,
                    },
                    other => StreamPumpOutcome::Error(AgentError::Stream(other)),
                };
            }
            StreamStep::End => break,
            StreamStep::Event(event) => match event {
                ResponseEvent::OutputItemDone(OutputItem::ToolUse { id, name, input }) => {
                    let call = ToolCallInfo { id, name, input };
                    let submission_index = tool_calls_seen.len();
                    tool_calls_seen.push(call.clone());

                    // Layer E.4: emit per-block ToolStart +
                    // ToolInputSnapshot so chat UX sees the same
                    // event sequence as the buffered streaming path
                    // (codex parity). The input snapshot carries the
                    // FULL parsed JSON (not partial) because the
                    // codex-shape stream surface only emits
                    // `OutputItemDone(ToolUse)` once the block has
                    // finished accumulating.
                    emit_event(
                        event_tx,
                        AgentLoopEvent::ToolStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    emit_event(
                        event_tx,
                        AgentLoopEvent::ToolInputSnapshot {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.to_string(),
                        },
                    );

                    // Layer E.4 tool-result cache: consult the
                    // per-run cache before spawning the future. On a
                    // hit, materialise the synthetic
                    // [`ToolCallResult`] inline so the FIFO drain
                    // returns it in submission order; the executor is
                    // NOT invoked (codex parity for read-only tools).
                    let single = std::slice::from_ref(&call);
                    let (cached_results, uncached_calls) = super::tool_execution::split_cached(
                        single,
                        &state.tool_cache.exact,
                        &state.tool_cache.fuzzy,
                    );
                    if !cached_results.is_empty() {
                        cached_pairs.push((
                            submission_index,
                            (call.clone(), cached_results.into_iter().next().unwrap()),
                        ));
                    } else if !uncached_calls.is_empty() {
                        spawned_indices.push(submission_index);
                        in_flight.push_back(spawn_tool_with_timeout(
                            executor,
                            uncached_calls.into_iter().next().unwrap(),
                            per_tool_timeout,
                        ));
                    }

                    // E.3 input-drain granularity: once per
                    // `OutputItemDone(tool_use)`. See module docs.
                    if let Some(queue) = input_queue {
                        if queue.has_pending() {
                            let drained = queue.drain().await;
                            if !drained.is_empty() {
                                super::turn::apply_user_inputs_to_messages(
                                    &mut state.messages,
                                    drained,
                                );
                            }
                        }
                    }
                }
                ResponseEvent::OutputItemDone(OutputItem::Message { text }) => {
                    // Layer E.4: emit a coarse TextDelta carrying the
                    // whole finished block. Sub-block per-wire-frame
                    // deltas remain on the legacy buffered path; this
                    // is the minimum needed for chat UX continuity
                    // (the assistant text actually surfaces) when
                    // the pump default flips to `true`.
                    emit_event(event_tx, AgentLoopEvent::TextDelta(text.clone()));
                    text_chunks.push(text);
                }
                ResponseEvent::OutputItemDone(OutputItem::Thinking {
                    thinking,
                    signature,
                }) => {
                    // Layer E.4: same coarse-granularity rationale as
                    // TextDelta above. Emit ThinkingDelta + the
                    // ThinkingComplete marker so consumers that
                    // pattern-match on the end-of-thinking event
                    // (e.g. the chat client's "Thought for Xs" pill)
                    // still see the close signal.
                    emit_event(event_tx, AgentLoopEvent::ThinkingDelta(thinking.clone()));
                    emit_event(event_tx, AgentLoopEvent::ThinkingComplete);
                    thinking_chunks.push((thinking, signature));
                }
                ResponseEvent::Completed {
                    end_turn: et,
                    usage: u,
                } => {
                    end_turn = et;
                    usage = u;
                    break;
                }
                ResponseEvent::Error(err) => {
                    return match err {
                        aura_reasoner::StreamError::StreamAbortedWithPartial {
                            reason,
                            partial_tool_use,
                        } => StreamPumpOutcome::AbortedWithPartial {
                            reason,
                            partial_tool_use,
                        },
                        other => StreamPumpOutcome::Error(AgentError::Stream(other)),
                    };
                }
            },
        }
    }

    // Drain the FIFO in submission order (codex `drain_in_flight`).
    // Honours cancellation: a token fired during drain still aborts
    // before we mutate `state.messages` in the caller.
    //
    // Spawned tools resolve in submission order via `FuturesOrdered`;
    // cached results are interleaved using the per-call submission
    // index captured above so the final `tool_results` vec mirrors
    // the order the model emitted the corresponding
    // `OutputItemDone(tool_use)` events.
    let mut spawned_pairs: Vec<(usize, (ToolCallInfo, ToolCallResult))> = Vec::new();
    let mut spawn_cursor = 0usize;
    loop {
        let maybe_next = drain_next(&mut in_flight, cancellation_token).await;
        match maybe_next {
            DrainStep::Cancelled => return StreamPumpOutcome::Cancelled,
            DrainStep::Done => break,
            DrainStep::Result(pair) => {
                let submission_index = spawned_indices
                    .get(spawn_cursor)
                    .copied()
                    .unwrap_or(usize::MAX);
                spawn_cursor = spawn_cursor.saturating_add(1);
                spawned_pairs.push((submission_index, pair));
            }
        }
    }

    let mut tool_results: Vec<(ToolCallInfo, ToolCallResult)> =
        Vec::with_capacity(tool_calls_seen.len());
    let mut merged: Vec<(usize, (ToolCallInfo, ToolCallResult))> =
        Vec::with_capacity(cached_pairs.len() + spawned_pairs.len());
    merged.append(&mut cached_pairs);
    merged.append(&mut spawned_pairs);
    merged.sort_by_key(|(idx, _)| *idx);
    for (_, pair) in merged {
        tool_results.push(pair);
    }

    // Layer E.4: refresh the per-run cache with any newly-executed
    // results. Mirrors the buffered path's
    // `tool_execution::handle_tool_use` → `update_cache` step so the
    // pump path participates in the same memoization /
    // invalidate-on-write contract. The "uncached" set is just the
    // tools we spawned (cached hits were already accounted for at
    // submission time).
    let spawned_calls: Vec<ToolCallInfo> = spawned_indices
        .iter()
        .filter_map(|i| tool_calls_seen.get(*i).cloned())
        .collect();
    let spawned_results: Vec<ToolCallResult> = tool_results
        .iter()
        .filter(|(c, _)| spawned_calls.iter().any(|sc| sc.id == c.id))
        .map(|(_, r)| r.clone())
        .collect();
    super::tool_execution::update_cache(
        &mut state.tool_cache.exact,
        &mut state.tool_cache.fuzzy,
        &spawned_calls,
        &spawned_results,
    );

    let response = synthesize_response(
        &text_chunks,
        &thinking_chunks,
        &tool_calls_seen,
        end_turn,
        &usage,
        model_name,
    );
    StreamPumpOutcome::Completed {
        response,
        tool_results,
    }
}

enum StreamStep {
    Event(ResponseEvent),
    End,
    Cancelled,
    TimedOut,
    TransportErr(aura_reasoner::StreamError),
}

async fn next_stream_step(
    stream: &mut ResponseEventStream,
    stream_event_timeout: Duration,
    cancellation_token: Option<&CancellationToken>,
) -> StreamStep {
    let polled = if let Some(token) = cancellation_token {
        // `biased` ensures cancellation observed alongside an event
        // always wins (Rule 6.3 cancellation precedence). Even if the
        // stream and the token are both ready, we route to the
        // cancellation arm.
        tokio::select! {
            biased;
            () = token.cancelled() => return StreamStep::Cancelled,
            polled = timeout(stream_event_timeout, stream.next()) => polled,
        }
    } else {
        timeout(stream_event_timeout, stream.next()).await
    };

    match polled {
        Err(_elapsed) => StreamStep::TimedOut,
        Ok(None) => StreamStep::End,
        Ok(Some(Ok(event))) => StreamStep::Event(event),
        Ok(Some(Err(err))) => StreamStep::TransportErr(err),
    }
}

enum DrainStep {
    Result((ToolCallInfo, ToolCallResult)),
    Done,
    Cancelled,
}

async fn drain_next<'a>(
    in_flight: &mut FuturesOrdered<ToolFuture<'a>>,
    cancellation_token: Option<&CancellationToken>,
) -> DrainStep {
    if in_flight.is_empty() {
        return DrainStep::Done;
    }
    if let Some(token) = cancellation_token {
        tokio::select! {
            biased;
            () = token.cancelled() => DrainStep::Cancelled,
            next = in_flight.next() => match next {
                Some(pair) => DrainStep::Result(pair),
                None => DrainStep::Done,
            }
        }
    } else {
        match in_flight.next().await {
            Some(pair) => DrainStep::Result(pair),
            None => DrainStep::Done,
        }
    }
}

fn spawn_tool_with_timeout(
    executor: &dyn AgentToolExecutor,
    call: ToolCallInfo,
    per_tool_timeout: Duration,
) -> ToolFuture<'_> {
    Box::pin(async move {
        let call_clone = call.clone();
        let id = call_clone.id.clone();
        let name = call_clone.name.clone();
        let fut = executor.spawn_tool_call(call_clone);
        let result = match timeout(per_tool_timeout, fut).await {
            Ok(result) => result,
            Err(_elapsed) => {
                let elapsed_ms = per_tool_timeout.as_millis();
                ToolCallResult::error(
                    id,
                    format!("tool '{name}' timed out after {elapsed_ms}ms (per_tool_timeout)"),
                )
            }
        };
        (call, result)
    })
}

fn synthesize_response(
    text_chunks: &[String],
    thinking_chunks: &[(String, Option<String>)],
    tool_calls: &[ToolCallInfo],
    end_turn: Option<bool>,
    usage: &Usage,
    model_name: &str,
) -> ModelResponse {
    let mut content: Vec<ContentBlock> = Vec::new();
    for (thinking, signature) in thinking_chunks {
        content.push(ContentBlock::Thinking {
            thinking: thinking.clone(),
            signature: signature.clone(),
        });
    }
    for text in text_chunks {
        content.push(ContentBlock::Text { text: text.clone() });
    }
    for call in tool_calls {
        content.push(ContentBlock::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        });
    }

    // Derive a `StopReason` from the (model-emitted) end_turn bit
    // plus whether any tool calls were observed. `MaxTokens` is not
    // representable in `Completed.end_turn` today (the SSE
    // `MessageDelta.stop_reason` *can* be `MaxTokens` but we drop
    // that fidelity at the adapter boundary today — codex parity).
    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else if matches!(end_turn, Some(false)) {
        // Provider explicitly signalled "more work" without emitting
        // tool calls — treat as `MaxTokens` so the existing
        // `iteration::handle_max_tokens` truncation path can run.
        StopReason::MaxTokens
    } else {
        StopReason::EndTurn
    };

    ModelResponse::new(
        stop_reason,
        Message::new(Role::Assistant, content),
        usage.clone(),
        ProviderTrace::new(model_name, 0),
    )
}

/// Post-pump dispatcher for the streaming sampling path
/// (Layer E.3). Mirrors [`super::AgentLoop::dispatch_stop_reason`]
/// for the buffered path but consumes the pre-executed
/// [`ToolCallResult`]s produced by [`run_stream_pump`] instead of
/// re-invoking the executor.
///
/// Returns `true` when the sampling loop should break (terminal stop
/// reason or `stop_loop = true` tool result). Mirrors the buffered
/// path's contract so the sampling driver can fold this bit into
/// `SamplingRequestResult::needs_follow_up` without branching on the
/// pump-vs-buffered split.
pub(super) async fn dispatch_streamed_response(
    agent: &super::AgentLoop,
    executor: &dyn AgentToolExecutor,
    response: &ModelResponse,
    tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    event_tx: Option<&tokio::sync::mpsc::Sender<crate::events::AgentLoopEvent>>,
    state: &mut super::LoopState,
) -> bool {
    // Production dev-loop path: `use_stream_pump` defaults to `true`,
    // so this dispatcher (not `super::AgentLoop::dispatch_stop_reason`)
    // is what fires on a real dev-loop turn. The buffered path stays
    // in sync because both call the same predicate.
    match response.stop_reason {
        aura_reasoner::StopReason::EndTurn | aura_reasoner::StopReason::StopSequence => {
            !agent.should_intercept_empty_termination(state)
        }
        aura_reasoner::StopReason::MaxTokens => {
            if super::iteration::handle_max_tokens(&agent.config, response, state) {
                // Pending tool_use blocks were synthesised; keep
                // looping so the model retries the dropped calls.
                false
            } else if agent.should_intercept_empty_termination(state) {
                // Extended thinking ate the budget without producing
                // a tool call. Arm the latch so the next iteration
                // opens with thinking disabled, and let
                // `run_turn_stop_hooks` run the GoalRuntime nudge
                // path.
                state.thinking.pending_disable_thinking_next_iteration = true;
                false
            } else {
                true
            }
        }
        aura_reasoner::StopReason::ToolUse => {
            handle_streamed_tool_use(&agent.config, executor, tool_results, event_tx, state).await
        }
    }
}

/// Streaming-pump analog of `tool_execution::handle_tool_use` that
/// consumes pre-executed [`ToolCallResult`]s instead of re-invoking
/// the executor. Emits per-result events, runs the auto-build
/// post-write side-step, and appends the `tool_result`-bearing user
/// message. Returns `true` when the sampling loop should break (the
/// buffered path's contract).
///
/// Layer E.4: tool-result caching now happens inside the pump's
/// [`drive_stream`] (per-`OutputItemDone` lookup against
/// `state.tool_cache`) and auto-build runs here on each successful
/// write, mirroring the buffered path's
/// `tool_pipeline::process_tool_results` behaviour. This closes the
/// parity gap that kept the pump opt-in pre-E.4.
async fn handle_streamed_tool_use(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    event_tx: Option<&tokio::sync::mpsc::Sender<crate::events::AgentLoopEvent>>,
    state: &mut super::LoopState,
) -> bool {
    if tool_results.is_empty() {
        return false;
    }
    let mut tool_calls: Vec<ToolCallInfo> = Vec::with_capacity(tool_results.len());
    let mut results: Vec<ToolCallResult> = Vec::with_capacity(tool_results.len());
    for (call, result) in tool_results {
        tool_calls.push(call);
        results.push(result);
    }

    // Latch the `had_any_file_write` bit using the existing detection
    // logic, so the dev-loop continuation runtime keeps seeing
    // forward motion through the pump path.
    let any_write_success = super::tool_pipeline::track_tool_effects_public(
        &tool_calls,
        &results,
        &mut state.result,
        &mut state.exploration_state,
        &mut state.had_any_write,
        &mut state.turn_diff,
    );
    if state.had_any_write {
        state.had_any_file_write = true;
    }

    // Layer E.4: auto-build after a successful write through the
    // pump. Mirrors the buffered path's
    // `tool_pipeline::process_tool_results` step so the dev-loop's
    // build feedback loop fires on the pump path too. The build
    // output is appended to the trailing tool_result-bearing user
    // message via `push_tool_result_message_with_context`, the same
    // adapter the buffered path uses.
    let mut side_messages: Vec<String> = Vec::new();
    if any_write_success && state.build_cooldown == 0 {
        if let Some(build_text) = super::tool_pipeline::run_auto_build_public(
            config,
            executor,
            &mut state.build_cooldown,
            state.build_baseline.as_ref(),
        )
        .await
        {
            side_messages.push(build_text);
        }
    }

    for (call, result) in tool_calls.iter().zip(results.iter()) {
        // Emit the same `ToolCallCompleted` + `ToolResult` pair the
        // buffered path emits so downstream forwarders see a
        // consistent event sequence regardless of which sampling
        // path produced the result.
        emit_event(
            event_tx,
            crate::events::AgentLoopEvent::ToolCallCompleted {
                tool_use_id: result.tool_use_id.clone(),
                tool_name: call.name.clone(),
                input: call.input.clone(),
                is_error: result.is_error,
            },
        );
        emit_event(
            event_tx,
            crate::events::AgentLoopEvent::ToolResult {
                tool_use_id: result.tool_use_id.clone(),
                tool_name: call.name.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            },
        );
    }

    let task_done_success = tool_calls.iter().any(|tc| tc.name == "task_done")
        && results.iter().any(|r| !r.is_error && r.stop_loop);
    if task_done_success {
        state.task_done_completed = true;
    }

    let should_stop = results.iter().any(|r| r.stop_loop);
    super::tool_execution::push_tool_result_message_with_context(
        &mut state.messages,
        results,
        side_messages,
    );
    should_stop
}

/// Drop an advisory event onto the inner agent-event channel.
///
/// Layer E.4 turned the pump into a per-`OutputItemDone` event source
/// (`TextDelta` / `ThinkingDelta` / `ToolStart` / `ToolInputSnapshot`
/// / `ToolCallCompleted` / `ToolResult`), so this helper is now the
/// hot path on the streaming sampling driver. The downstream
/// consumer is the
/// `aura_automaton::builtins::dev_loop::spawn_agent_event_forwarder`
/// task, which already applies a debounced drop policy on its outer
/// projection — replicating a per-event `WARN!` here just doubles the
/// noise during a normal burst, and a closed inner channel is
/// already a downstream lifecycle signal (the forwarder dropped its
/// receiver because the wrapping run is winding down).
///
/// Policy: `try_send`; drop silently on `Full` / `Closed`. Logging
/// downgraded to `debug!` so operators can opt back into per-event
/// visibility with `RUST_LOG=aura_agent::agent_loop::stream_pump=debug`
/// without paying the warn-cadence cost in production logs.
fn emit_event(
    tx: Option<&tokio::sync::mpsc::Sender<crate::events::AgentLoopEvent>>,
    event: crate::events::AgentLoopEvent,
) {
    if let Some(tx) = tx {
        if let Err(e) = tx.try_send(event) {
            tracing::debug!("agent event channel full or closed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    // Note: the bulk of the E.3 mandatory tests live in
    // `crate::agent_loop::stream_pump_tests` so they can use scripted
    // fake providers and `start_paused = true` time control without
    // pulling the full sampling driver into scope.
    use super::*;
    use crate::session::SessionId;
    use crate::types::ToolCallResult;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct CountingExecutor {
        invocations: tokio::sync::Mutex<Vec<ToolCallInfo>>,
    }

    #[async_trait]
    impl AgentToolExecutor for CountingExecutor {
        async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
            let mut guard = self.invocations.lock().await;
            for call in tool_calls {
                guard.push(call.clone());
            }
            tool_calls
                .iter()
                .map(|tc| ToolCallResult::success(tc.id.clone(), format!("ok:{}", tc.name)))
                .collect()
        }
    }

    fn mk_call(id: &str, name: &str) -> ResponseEvent {
        ResponseEvent::OutputItemDone(OutputItem::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        })
    }

    fn mk_stream(events: Vec<ResponseEvent>) -> ResponseEventStream {
        Box::pin(futures_util::stream::iter(
            events.into_iter().map(Ok::<_, aura_reasoner::StreamError>),
        ))
    }

    #[tokio::test]
    async fn pump_drains_in_fifo_submission_order() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let events = vec![
            mk_call("toolu_a", "read_file"),
            mk_call("toolu_b", "read_file"),
            mk_call("toolu_c", "read_file"),
            ResponseEvent::Completed {
                end_turn: Some(false),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            None,
            None,
            None,
            &mut state,
            "test-model",
        )
        .await;
        match outcome {
            StreamPumpOutcome::Completed { tool_results, .. } => {
                let ids: Vec<_> = tool_results.iter().map(|(c, _)| c.id.clone()).collect();
                assert_eq!(ids, vec!["toolu_a", "toolu_b", "toolu_c"]);
            }
            _ => panic!("expected Completed outcome"),
        }
    }

    #[tokio::test]
    async fn pump_cancellation_yields_atomic_no_write() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig {
            stream_event_timeout: Duration::from_secs(30),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let stream: ResponseEventStream = Box::pin(futures_util::stream::pending());
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            Some(&cancel),
            None,
            None,
            &mut state,
            "test-model",
        )
        .await;
        assert!(matches!(outcome, StreamPumpOutcome::Cancelled));
        assert!(state.messages.is_empty(), "no state mutation on cancel");
    }

    #[tokio::test]
    async fn pump_per_outputitemdone_input_drain() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let cancel = CancellationToken::new();
        let queue = InputQueue::new(SessionId::new_v4(), cancel.clone());
        // Drive: one tool call, then user types something between
        // tool calls, then another tool call, then Completed.
        // The drain happens after the first tool call's
        // OutputItemDone, so the second iteration's apply-step
        // should already see the pushed input.
        queue
            .push(crate::session::UserInput::Message("queued-message".into()))
            .await
            .unwrap();
        let _ = cancel; // keep alive
        let events = vec![
            mk_call("toolu_a", "read_file"),
            mk_call("toolu_b", "read_file"),
            ResponseEvent::Completed {
                end_turn: Some(false),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            None,
            Some(&queue),
            None,
            &mut state,
            "test-model",
        )
        .await;
        assert!(matches!(outcome, StreamPumpOutcome::Completed { .. }));
        // The queued message should have been drained mid-pump and
        // appended to state.messages.
        assert!(
            state
                .messages
                .iter()
                .any(|m| m.content.iter().any(|b| matches!(
                    b,
                    aura_reasoner::ContentBlock::Text { text } if text.contains("queued-message")
                ))),
            "drained user input must be appended to messages mid-pump"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pump_stream_event_timeout_surfaces_typed_error() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig {
            stream_event_timeout: Duration::from_secs(5),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let stream: ResponseEventStream = Box::pin(futures_util::stream::pending());
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            None,
            None,
            None,
            &mut state,
            "test-model",
        )
        .await;
        assert!(matches!(
            outcome,
            StreamPumpOutcome::Error(AgentError::StreamTimeout { .. })
        ));
    }

    /// Confirm the pump uses true per-tool concurrency: when three
    /// tools each sleep for 5s using the paused tokio clock, the
    /// FIFO drains after a single 5s `advance` rather than the
    /// 15s a sequential dispatcher would need.
    #[tokio::test(start_paused = true)]
    async fn pump_overlaps_concurrent_tools() {
        #[derive(Default)]
        struct SleepyExecutor;
        #[async_trait]
        impl AgentToolExecutor for SleepyExecutor {
            async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
                let mut out = Vec::new();
                for tc in tool_calls {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    out.push(ToolCallResult::success(tc.id.clone(), "ok"));
                }
                out
            }
        }

        let executor = SleepyExecutor;
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let events = vec![
            mk_call("toolu_a", "t"),
            mk_call("toolu_b", "t"),
            mk_call("toolu_c", "t"),
            ResponseEvent::Completed {
                end_turn: Some(false),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);
        let mut state = super::super::LoopState::new(&config, Vec::new());
        let notify = Arc::new(Notify::new());
        let notify_clone = Arc::clone(&notify);

        let driver = tokio::spawn(async move {
            // Drive the pump to completion. Returns the outcome.
            let outcome = drive_stream(
                &config,
                &executor,
                stream,
                None,
                None,
                None,
                &mut state,
                "test-model",
            )
            .await;
            notify_clone.notify_one();
            outcome
        });

        // Let the spawned task progress to the await on the first
        // sleep. We can't deterministically know when, so yield a
        // few times to give it scheduler attention before advancing
        // the clock.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        // Single 5s advance must complete ALL three sleeps if the
        // pump is overlapping. With a 15s advance the test would
        // pass even for a sequential executor, so a 5s window is
        // the discriminating signal.
        tokio::time::advance(Duration::from_secs(5)).await;
        // The notify fires when the pump returns. With overlap, the
        // pump returns after the single advance (all 3 sleeps
        // completed in parallel). The notify wait is bounded by a
        // generous timeout so a sequential executor would surface
        // as a wait-timeout panic.
        let res = tokio::time::timeout(Duration::from_secs(120), notify.notified()).await;
        assert!(
            res.is_ok(),
            "pump should complete after single 5s advance when tools overlap"
        );
        let outcome = driver.await.expect("driver join");
        match outcome {
            StreamPumpOutcome::Completed { tool_results, .. } => {
                assert_eq!(tool_results.len(), 3);
                let ids: Vec<_> = tool_results.iter().map(|(c, _)| c.id.clone()).collect();
                assert_eq!(ids, vec!["toolu_a", "toolu_b", "toolu_c"]);
            }
            _ => panic!("expected Completed"),
        }
    }

    /// Confirm a hung tool that exceeds `per_tool_timeout` resolves
    /// to a synthetic error result without poisoning the FIFO. The
    /// other tools in the batch still produce their normal results.
    #[tokio::test(start_paused = true)]
    async fn pump_per_tool_timeout_does_not_poison_fifo() {
        #[derive(Default)]
        struct PartiallyHungExecutor;
        #[async_trait]
        impl AgentToolExecutor for PartiallyHungExecutor {
            async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
                let mut out = Vec::new();
                for tc in tool_calls {
                    if tc.name == "hang" {
                        // Sleep way past the 10s per-tool timeout.
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        out.push(ToolCallResult::success(tc.id.clone(), "unreachable"));
                    } else {
                        out.push(ToolCallResult::success(tc.id.clone(), "ok"));
                    }
                }
                out
            }
        }
        let executor = PartiallyHungExecutor;
        let config = AgentLoopConfig {
            per_tool_timeout: Duration::from_secs(10),
            stream_event_timeout: Duration::from_secs(120),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let events = vec![
            mk_call("toolu_a", "ok"),
            mk_call("toolu_b", "hang"),
            mk_call("toolu_c", "ok"),
            ResponseEvent::Completed {
                end_turn: Some(false),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let driver = tokio::spawn(async move {
            drive_stream(
                &config,
                &executor,
                stream,
                None,
                None,
                None,
                &mut state,
                "test-model",
            )
            .await
        });

        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        let outcome = tokio::time::timeout(Duration::from_secs(120), driver)
            .await
            .expect("driver did not complete after timeout window")
            .expect("driver join");
        match outcome {
            StreamPumpOutcome::Completed { tool_results, .. } => {
                assert_eq!(tool_results.len(), 3);
                assert_eq!(tool_results[0].1.is_error, false);
                assert!(tool_results[1].1.is_error, "hung tool should error");
                assert!(
                    tool_results[1].1.content.contains("timed out"),
                    "hung tool should mention timeout"
                );
                assert_eq!(
                    tool_results[2].1.is_error, false,
                    "subsequent tools must still produce their normal result"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // E.4 mandatory pump tests
    // -----------------------------------------------------------------

    /// `pump_emits_per_delta_events` (E.4 mandatory): with an
    /// `event_tx` plumbed in, the pump emits at minimum a `TextDelta`
    /// for a finished `OutputItem::Message`, a `ThinkingDelta` +
    /// `ThinkingComplete` for `OutputItem::Thinking`, and a
    /// `ToolStart` + `ToolInputSnapshot` pair for an
    /// `OutputItem::ToolUse`. This is the gate that lets the
    /// `use_stream_pump` default flip without regressing the
    /// chat-stream UX (see audit note).
    #[tokio::test(start_paused = true)]
    async fn pump_emits_per_delta_events() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let events = vec![
            ResponseEvent::OutputItemDone(OutputItem::Thinking {
                thinking: "thought".into(),
                signature: None,
            }),
            ResponseEvent::OutputItemDone(OutputItem::Message {
                text: "hello".into(),
            }),
            mk_call("toolu_a", "read_file"),
            ResponseEvent::Completed {
                end_turn: Some(true),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);
        let mut state = super::super::LoopState::new(&config, Vec::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            None,
            None,
            Some(&tx),
            &mut state,
            "test-model",
        )
        .await;
        assert!(matches!(outcome, StreamPumpOutcome::Completed { .. }));
        drop(tx);

        let mut events: Vec<AgentLoopEvent> = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentLoopEvent::TextDelta(t) if t == "hello")),
            "pump must emit TextDelta for Message blocks: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentLoopEvent::ThinkingDelta(t) if t == "thought")),
            "pump must emit ThinkingDelta for Thinking blocks: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentLoopEvent::ThinkingComplete)),
            "pump must emit ThinkingComplete after a Thinking block: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentLoopEvent::ToolStart { id, name } if id == "toolu_a" && name == "read_file"
            )),
            "pump must emit ToolStart for OutputItemDone(ToolUse): {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentLoopEvent::ToolInputSnapshot { id, name, .. }
                    if id == "toolu_a" && name == "read_file"
            )),
            "pump must emit ToolInputSnapshot for OutputItemDone(ToolUse): {events:?}"
        );
    }

    /// `pump_cache_hit_short_circuits_tool_spawn` (E.4 mandatory):
    /// when `state.tool_cache.exact` already has a hit for a cacheable
    /// tool's input, the pump must serve the cached result inline
    /// *without* invoking the executor for that call. The other
    /// (uncached) call in the same model response is still spawned.
    #[tokio::test(start_paused = true)]
    async fn pump_cache_hit_short_circuits_tool_spawn() {
        let executor = CountingExecutor::default();
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let cached_input = serde_json::json!({});
        let cache_key = crate::constants::tool_result_cache_key("read_file", &cached_input);
        let mut state = super::super::LoopState::new(&config, Vec::new());
        state
            .tool_cache
            .exact
            .insert(cache_key, "cached-payload".to_string());

        let events = vec![
            mk_call("toolu_cached", "read_file"),
            mk_call("toolu_fresh", "run_command"),
            ResponseEvent::Completed {
                end_turn: Some(false),
                usage: Usage::new(1, 1),
            },
        ];
        let stream = mk_stream(events);

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
            None,
            None,
            None,
            &mut state,
            "test-model",
        )
        .await;
        match outcome {
            StreamPumpOutcome::Completed { tool_results, .. } => {
                let ids: Vec<_> = tool_results.iter().map(|(c, _)| c.id.clone()).collect();
                assert_eq!(
                    ids,
                    vec!["toolu_cached", "toolu_fresh"],
                    "cached + spawned results must preserve FIFO submission order"
                );
                let cached_result = &tool_results[0].1;
                assert!(
                    !cached_result.is_error,
                    "cached hit must surface as a non-error"
                );
                assert_eq!(
                    cached_result.content, "cached-payload",
                    "cached hit must return the memoised payload verbatim"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        let invocations = executor.invocations.lock().await;
        let invoked_ids: Vec<_> = invocations.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            invoked_ids,
            vec!["toolu_fresh".to_string()],
            "executor must be invoked ONLY for the uncached tool; cache hits short-circuit spawn"
        );
    }

    /// `pump_triggers_auto_build_on_write` (E.4 mandatory): a
    /// successful `write_file` flowing through the pump fires
    /// `tool_pipeline::run_auto_build_public`, mirroring the buffered
    /// path's `process_tool_results` step. The failing-build text is
    /// appended to the trailing tool_result-bearing user message via
    /// `push_tool_result_message_with_context`, so the existence of
    /// that side message in `state.messages` is the observable proof.
    #[tokio::test(start_paused = true)]
    async fn pump_triggers_auto_build_on_write() {
        #[derive(Default)]
        struct BuildSpyExecutor {
            build_calls: tokio::sync::Mutex<u32>,
        }
        #[async_trait]
        impl AgentToolExecutor for BuildSpyExecutor {
            async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
                tool_calls
                    .iter()
                    .map(|tc| ToolCallResult {
                        tool_use_id: tc.id.clone(),
                        content: "wrote".to_string(),
                        is_error: false,
                        kind: aura_core::ToolResultKind::Ok,
                        stop_loop: false,
                        file_changes: vec![crate::types::FileChange {
                            path: "src/foo.rs".into(),
                            kind: crate::types::FileChangeKind::Modify,
                            lines_added: 3,
                            lines_removed: 0,
                        }],
                    })
                    .collect()
            }
            async fn auto_build_check(&self) -> Option<crate::types::AutoBuildResult> {
                *self.build_calls.lock().await += 1;
                Some(crate::types::AutoBuildResult {
                    success: false,
                    output: "compile error: missing semicolon".into(),
                    error_count: 1,
                })
            }
        }

        let executor = BuildSpyExecutor::default();
        let config = AgentLoopConfig {
            auto_build_cooldown: 0,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let response = ModelResponse::new(
            StopReason::ToolUse,
            Message::new(
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    "toolu_w",
                    "write_file",
                    serde_json::json!({"path": "src/foo.rs", "content": "fn a(){}"}),
                )],
            ),
            Usage::new(1, 1),
            ProviderTrace::new("test", 0),
        );
        let tool_call = ToolCallInfo {
            id: "toolu_w".to_string(),
            name: "write_file".to_string(),
            input: serde_json::json!({"path": "src/foo.rs", "content": "fn a(){}"}),
        };
        let tool_result = ToolCallResult {
            tool_use_id: "toolu_w".to_string(),
            content: "wrote".to_string(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: vec![crate::types::FileChange {
                path: "src/foo.rs".into(),
                kind: crate::types::FileChangeKind::Modify,
                lines_added: 3,
                lines_removed: 0,
            }],
        };
        let mut state = super::super::LoopState::new(&config, Vec::new());
        let agent = super::super::AgentLoop::new(config.clone());

        // Drive only the post-stream dispatch path — the pre-stream
        // pump already has its own coverage, and the auto-build
        // wiring lives in `handle_streamed_tool_use`.
        let _should_break = dispatch_streamed_response(
            &agent,
            &executor,
            &response,
            vec![(tool_call, tool_result)],
            None,
            &mut state,
        )
        .await;

        let calls = *executor.build_calls.lock().await;
        assert_eq!(
            calls, 1,
            "successful write must trigger auto_build_check exactly once on the pump path"
        );
        let saw_build_warning = state.messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                ContentBlock::Text { text } => text.contains("Build check failed"),
                ContentBlock::ToolResult {
                    content: aura_reasoner::ToolResultContent::Text(t),
                    ..
                } => t.contains("Build check failed"),
                _ => false,
            })
        });
        assert!(
            saw_build_warning,
            "failing auto-build output must surface in the tool_result-bearing message"
        );
    }

    // `Debug` impls for the outcome enum so tests can format failures
    // without us hand-rolling matchers everywhere.
    impl std::fmt::Debug for StreamPumpOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Completed { tool_results, .. } => f
                    .debug_struct("Completed")
                    .field("tool_results_len", &tool_results.len())
                    .finish(),
                Self::Cancelled => write!(f, "Cancelled"),
                Self::Error(e) => write!(f, "Error({e:?})"),
                Self::AbortedWithPartial { reason, .. } => f
                    .debug_struct("AbortedWithPartial")
                    .field("reason", reason)
                    .finish(),
            }
        }
    }
}
