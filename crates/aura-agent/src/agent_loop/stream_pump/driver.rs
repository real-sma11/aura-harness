//! Inner stream driver: event-loop body and `FuturesOrdered` drain.
//!
//! Separated from the public [`super::run_stream_pump`] entry so the
//! unit tests can hand it a hand-rolled `ResponseEventStream` without
//! a real `ModelProvider`. The retry/backoff envelope that wraps a
//! sequence of `drive_stream` attempts lives one level up in
//! [`super`].
//!
//! Layer E.4: takes an optional `event_tx` and emits per-`OutputItemDone`
//! equivalents of the legacy `streaming::emit_stream_event` deltas
//! (`TextDelta` / `ThinkingDelta` / `ToolStart` / `ToolInputSnapshot`)
//! so consumers of the streaming sampling pump observe the same
//! event surface they see on the buffered path. Granularity is at
//! the block boundary (codex-shape `OutputItemDone` arrives once per
//! finished block) rather than per-wire-frame — chat UI continuity
//! for the pump-default flip; sub-block per-token deltas remain on
//! the legacy buffered path until a follow-up wires a tap stream.

use std::pin::Pin;
use std::time::{Duration, Instant};

use aura_model_reasoner::{OutputItem, ResponseEvent, ResponseEventStream, StreamPhase, Usage};
use futures_util::stream::FuturesOrdered;
use futures_util::StreamExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::console;
use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};
use crate::AgentError;

use super::synthesize::synthesize_response;
use super::{emit_event, StreamPumpOutcome};

/// Boxed per-tool future spawned into [`FuturesOrdered`] inside the
/// pump. Carries the originating [`ToolCallInfo`] alongside the
/// result so the drain loop can reassemble FIFO order without an
/// auxiliary lookup. Aliased per `clippy::type_complexity`.
type ToolFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = (ToolCallInfo, ToolCallResult)> + Send + 'a>>;

/// Drive a single `ResponseEventStream` to terminal completion.
///
/// One attempt of the retry envelope in [`super::run_stream_pump`].
/// Returns whichever [`StreamPumpOutcome`] terminated the loop body:
/// `Completed` on a clean `Completed` event, `Cancelled` /
/// `AbortedWithPartial` / `Error` on the matching short-circuit
/// arms. Per the module-level invariants in [`super`], no
/// state.messages mutation happens on the cancel or error arms.
pub(super) async fn drive_stream(
    ctx: super::StreamPumpCtx<'_>,
    mut stream: ResponseEventStream,
    state: &mut super::super::LoopState,
    model_name: &str,
) -> StreamPumpOutcome {
    let super::StreamPumpCtx {
        config,
        cancellation_token,
        event_tx,
        ..
    } = ctx;
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
    // Liveness/phase trail: `stream_started` stamps the `t+` clock on
    // phase-transition lines, and `logged_phase` debounces the trail
    // so only an actual phase change emits a line (not every delta).
    let stream_started = Instant::now();
    let mut logged_phase: Option<StreamPhase> = None;

    loop {
        let next_step =
            next_stream_step(&mut stream, stream_event_timeout, cancellation_token).await;
        match next_step {
            StreamStep::Cancelled => {
                // Mid-stream cancellation. If the model has already
                // emitted any `tool_use` blocks we MUST synthesise
                // paired `[CANCELLED]` tool_results so the downstream
                // `dispatch_streamed_response` →
                // `push_tool_result_message_with_context` step can
                // close the Anthropic `tool_use ↔ tool_result`
                // adjacency contract before the loop breaks. Pre-fix
                // this returned a bare `Cancelled` and the conversation
                // history ended with an orphaned assistant `tool_use`
                // block — invalid on the next sampling call.
                // See `pipeline_cancellation_mid_tool_execution_aborts_loop`.
                if tool_calls_seen.is_empty() {
                    return StreamPumpOutcome::Cancelled;
                }
                let snap = CancelledSnapshot {
                    text_chunks: &text_chunks,
                    thinking_chunks: &thinking_chunks,
                    tool_calls_seen: &tool_calls_seen,
                    cached_pairs: &cached_pairs,
                    spawned_indices: &spawned_indices,
                    end_turn,
                    usage: &usage,
                    model_name,
                };
                return cancelled_outcome(&snap, Vec::new());
            }
            StreamStep::TimedOut => {
                let elapsed_ms = stream_event_timeout
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                // Turn the previously-silent stall into an actionable
                // box: which phase the stream died in and how much
                // content had streamed before it went quiet.
                let thinking_bytes = thinking_chunks.iter().map(|(t, _)| t.len()).sum();
                let text_bytes = text_chunks.iter().map(String::len).sum();
                console::stream_timeout_block(&console::StreamTimeoutView {
                    elapsed_ms,
                    last_phase: logged_phase,
                    thinking_bytes,
                    text_bytes,
                });
                return StreamPumpOutcome::Error(AgentError::StreamTimeout { elapsed_ms });
            }
            StreamStep::TransportErr(err) => {
                return match err {
                    aura_model_reasoner::StreamError::StreamAbortedWithPartial {
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
                    if let Some(err) = handle_tool_use_event(
                        ctx,
                        ToolCallInfo { id, name, input },
                        DriveLocals {
                            tool_calls_seen: &mut tool_calls_seen,
                            cached_pairs: &mut cached_pairs,
                            spawned_indices: &mut spawned_indices,
                            in_flight: &mut in_flight,
                        },
                        per_tool_timeout,
                        state,
                    )
                    .await
                    {
                        return err;
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
                        aura_model_reasoner::StreamError::StreamAbortedWithPartial {
                            reason,
                            partial_tool_use,
                        } => StreamPumpOutcome::AbortedWithPartial {
                            reason,
                            partial_tool_use,
                        },
                        other => StreamPumpOutcome::Error(AgentError::Stream(other)),
                    };
                }
                ResponseEvent::Keepalive(phase) => {
                    // A ping or intra-block delta. Reaching this arm
                    // already reset the per-event liveness timeout
                    // (the stream yielded an item, so the loop
                    // re-enters `next_stream_step` with a fresh
                    // window). A genuine content-phase change also
                    // emits one transcript line so a long turn shows
                    // its progress. Pings are pure liveness and stay
                    // off the transcript to keep it quiet.
                    if phase != StreamPhase::Ping && logged_phase != Some(phase) {
                        console::stream_phase_line(phase, stream_started.elapsed());
                        logged_phase = Some(phase);
                    }
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
            DrainStep::Cancelled => {
                // Mid-drain cancellation. Drop the still-pending
                // `in_flight` futures (cancellation-safe — they were
                // wrapped in `tokio::time::timeout` and the executor
                // borrow ends here) and synthesise `[CANCELLED]`
                // tool_results for every `tool_calls_seen` entry that
                // has NOT yet produced a paired result. The downstream
                // dispatch tail (`dispatch_streamed_response` →
                // `push_tool_result_message_with_context`) then closes
                // the Anthropic `tool_use ↔ tool_result` adjacency
                // contract before the loop breaks on the synthesised
                // `stop_loop = true` flag. Pre-fix this returned a
                // bare `Cancelled` and let `executor.execute(...)` run
                // to natural completion — the regression contract is
                // `pipeline_cancellation_mid_tool_execution_aborts_loop`.
                let snap = CancelledSnapshot {
                    text_chunks: &text_chunks,
                    thinking_chunks: &thinking_chunks,
                    tool_calls_seen: &tool_calls_seen,
                    cached_pairs: &cached_pairs,
                    spawned_indices: &spawned_indices,
                    end_turn,
                    usage: &usage,
                    model_name,
                };
                return cancelled_outcome(&snap, spawned_pairs);
            }
            DrainStep::Done => break,
            DrainStep::Result(pair) => {
                let submission_index = spawned_indices
                    .get(spawn_cursor)
                    .copied()
                    .unwrap_or(usize::MAX);
                spawn_cursor = spawn_cursor.saturating_add(1);
                spawned_pairs.push((submission_index, *pair));
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
    super::super::tool_execution::update_cache(
        &mut state.tool_cache,
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

/// Phase 8 wrapper bundling the four mutable cursor buffers that
/// [`drive_stream`] hands to [`handle_tool_use_event`]: the running
/// `tool_calls_seen` FIFO, the synchronously materialised cache
/// hits, the per-call spawn-index trace, and the `FuturesOrdered`
/// itself. Grouping them keeps the helper under the clippy ceiling
/// without splitting the FIFO ordering invariant across helpers —
/// every field stays a borrowed mutation target.
pub(super) struct DriveLocals<'a, 'b> {
    pub(super) tool_calls_seen: &'b mut Vec<ToolCallInfo>,
    pub(super) cached_pairs: &'b mut Vec<(usize, (ToolCallInfo, ToolCallResult))>,
    pub(super) spawned_indices: &'b mut Vec<usize>,
    pub(super) in_flight: &'b mut FuturesOrdered<ToolFuture<'a>>,
}

/// Handle one `OutputItemDone(ToolUse)` event: circling-read gate,
/// per-run cache lookup, spawn-or-serve, and per-tool-result
/// input-queue drain. Returns `Some(StreamPumpOutcome)` only when an
/// invariant-violation early-return must propagate to [`drive_stream`]
/// (e.g. the per-run cache partition contract is broken). On the
/// happy path returns `None` and pushes the call into the FIFO /
/// cached_pairs bookkeeping.
async fn handle_tool_use_event<'a, 'b>(
    ctx: super::StreamPumpCtx<'a>,
    call: ToolCallInfo,
    locals: DriveLocals<'a, 'b>,
    per_tool_timeout: Duration,
    state: &mut super::super::LoopState,
) -> Option<StreamPumpOutcome> {
    let super::StreamPumpCtx {
        config: _,
        executor,
        cancellation_token: _,
        input_queue,
        event_tx,
    } = ctx;
    let DriveLocals {
        tool_calls_seen,
        cached_pairs,
        spawned_indices,
        in_flight,
    } = locals;
    let submission_index = tool_calls_seen.len();
    tool_calls_seen.push(call.clone());

    // Layer E.4: emit per-block ToolStart + ToolInputSnapshot so chat
    // UX sees the same event sequence as the buffered streaming path
    // (codex parity). The input snapshot carries the FULL parsed JSON
    // (not partial) because the codex-shape stream surface only emits
    // `OutputItemDone(ToolUse)` once the block has finished
    // accumulating.
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

    // Circling read gate: before cache lookup or executor spawn,
    // reject duplicate `read_file` paths once the steering layer
    // has latched a read-only loop. This keeps the streaming pump
    // in lockstep with the buffered dispatcher.
    let single = std::slice::from_ref(&call);
    let (blocked_reads, allowed_calls) =
        super::super::tool_pipeline::partition_circling_duplicate_reads(single, state);
    if let Some(blocked) = blocked_reads.into_iter().next() {
        cached_pairs.push((submission_index, (call, blocked)));
        return None;
    }

    // Layer E.4 tool-result cache: consult the per-run cache before
    // spawning the future. On a hit, materialise the synthetic
    // [`ToolCallResult`] inline so the FIFO drain returns it in
    // submission order; the executor is NOT invoked (codex parity
    // for read-only tools).
    //
    // Cache invariant (Rule 4.1 / 4.3): `split_cached` partitions
    // `allowed_calls` (a single-element slice here) into
    // `cached_results` and `uncached_calls` such that every input
    // call ends up in exactly one bucket. So when either bucket is
    // non-empty its `into_iter().next()` MUST yield an element; a
    // `None` here is a true partition invariant violation. Surface
    // it as a fatal `LlmCallError` via `AgentError::Internal` rather
    // than panicking — the `Error` outcome is folded into
    // `LlmCallError::Fatal` upstream in `sampling::run_sampling_request`.
    let (cached_results, uncached_calls) =
        super::super::tool_execution::split_cached(&allowed_calls, &state.tool_cache);
    if !cached_results.is_empty() {
        let Some(first) = cached_results.into_iter().next() else {
            return Some(StreamPumpOutcome::Error(AgentError::Internal(
                "stream pump cache invariant: cached_results was non-empty but yielded no element \
                 (split_cached partition contract broken)"
                    .to_string(),
            )));
        };
        cached_pairs.push((submission_index, (call, first)));
    } else if !uncached_calls.is_empty() {
        let Some(first) = uncached_calls.into_iter().next() else {
            return Some(StreamPumpOutcome::Error(AgentError::Internal(
                "stream pump cache invariant: uncached_calls was non-empty but yielded no element \
                 (split_cached partition contract broken)"
                    .to_string(),
            )));
        };
        spawned_indices.push(submission_index);
        in_flight.push_back(spawn_tool_with_timeout(executor, first, per_tool_timeout));
    }

    // E.3 input-drain granularity: once per `OutputItemDone(tool_use)`.
    // See module-level docs.
    if let Some(queue) = input_queue {
        if queue.has_pending() {
            let drained = queue.drain().await;
            if !drained.is_empty() {
                super::super::turn::apply_user_inputs_to_messages(&mut state.messages, drained);
            }
        }
    }
    None
}

enum StreamStep {
    Event(ResponseEvent),
    End,
    Cancelled,
    TimedOut,
    TransportErr(aura_model_reasoner::StreamError),
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
    // Boxed: `ToolCallResult` carries an optional screenshot image, so
    // the pair is large relative to the unit `Done` / `Cancelled`
    // variants. Boxing keeps the enum small (clippy::large_enum_variant).
    Result(Box<(ToolCallInfo, ToolCallResult)>),
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
                Some(pair) => DrainStep::Result(Box::new(pair)),
                None => DrainStep::Done,
            }
        }
    } else {
        match in_flight.next().await {
            Some(pair) => DrainStep::Result(Box::new(pair)),
            None => DrainStep::Done,
        }
    }
}

/// Build a `Completed` outcome that carries `[CANCELLED]` tool_results
/// for every emitted tool_use that did NOT yet produce a paired
/// result. Drains `cached_pairs` + `spawned_pairs` first (already-
/// resolved entries keep their real result), then synthesises
/// `stop_loop = true` cancelled entries for the rest so the downstream
/// dispatcher breaks the loop on the same iteration.
///
/// Returning `Completed` (instead of `Cancelled`) is deliberate: the
/// sampling driver's `Completed` arm runs
/// `iteration::accumulate_response` (pushes the assistant message with
/// the seen tool_use blocks) and `dispatch_streamed_response` (pushes
/// the paired tool_result-bearing user message). Routing through that
/// path is the only way to satisfy Anthropic's `tool_use ↔ tool_result`
/// adjacency contract on a mid-tool cancel — a bare `Cancelled` would
/// leave an orphaned assistant `tool_use` block at the tail of the
/// transcript and the next sampling call would be structurally invalid.
/// Phase 8 wrapper around the immutable observations [`drive_stream`]
/// accumulated before a mid-flight cancellation. Bundles the four
/// `&[…]` slices plus the response-synthesis state needed to fold a
/// partial transcript into a [`StreamPumpOutcome::Completed`] with
/// `[CANCELLED]` tool_results. Owned `spawned_pairs` stays a
/// positional argument so it can be moved into the merge step.
struct CancelledSnapshot<'a> {
    text_chunks: &'a [String],
    thinking_chunks: &'a [(String, Option<String>)],
    tool_calls_seen: &'a [ToolCallInfo],
    cached_pairs: &'a [(usize, (ToolCallInfo, ToolCallResult))],
    spawned_indices: &'a [usize],
    end_turn: Option<bool>,
    usage: &'a Usage,
    model_name: &'a str,
}

fn cancelled_outcome(
    snap: &CancelledSnapshot<'_>,
    mut spawned_pairs: Vec<(usize, (ToolCallInfo, ToolCallResult))>,
) -> StreamPumpOutcome {
    use std::collections::HashSet;
    let &CancelledSnapshot {
        text_chunks,
        thinking_chunks,
        tool_calls_seen,
        cached_pairs,
        spawned_indices,
        end_turn,
        usage,
        model_name,
    } = snap;

    let already: HashSet<usize> = cached_pairs
        .iter()
        .map(|(idx, _)| *idx)
        .chain(spawned_pairs.iter().map(|(idx, _)| *idx))
        .collect();

    let still_pending: HashSet<usize> = spawned_indices
        .iter()
        .copied()
        .filter(|idx| !already.contains(idx))
        .collect();

    for idx in still_pending {
        if let Some(call) = tool_calls_seen.get(idx) {
            let result = ToolCallResult {
                tool_use_id: call.id.clone(),
                content:
                    "[CANCELLED] Tool execution was cancelled by the user before the tool returned."
                        .to_string(),
                is_error: true,
                kind: aura_core_types::ToolResultKind::AgentError,
                stop_loop: true,
                image: None,
                file_changes: Vec::new(),
            };
            spawned_pairs.push((idx, (call.clone(), result)));
        }
    }

    let mut tool_results: Vec<(ToolCallInfo, ToolCallResult)> =
        Vec::with_capacity(tool_calls_seen.len());
    let mut merged: Vec<(usize, (ToolCallInfo, ToolCallResult))> =
        Vec::with_capacity(cached_pairs.len() + spawned_pairs.len());
    merged.extend(cached_pairs.iter().cloned());
    merged.append(&mut spawned_pairs);
    merged.sort_by_key(|(idx, _)| *idx);
    for (_, pair) in merged {
        tool_results.push(pair);
    }

    let response = synthesize_response(
        text_chunks,
        thinking_chunks,
        tool_calls_seen,
        end_turn,
        usage,
        model_name,
    );
    StreamPumpOutcome::Completed {
        response,
        tool_results,
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
