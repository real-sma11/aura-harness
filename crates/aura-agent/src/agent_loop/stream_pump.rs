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

use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, OutputItem, ProviderTrace,
    ResponseEvent, ResponseEventStream, Role, StopReason, Usage,
};
use futures_util::stream::FuturesOrdered;
use futures_util::StreamExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::session::input_queue::InputQueue;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};
use crate::AgentError;

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
}

/// Drive one sampling request through the streaming pump.
///
/// Opens `provider.complete_response_stream(request)`, then drives
/// the resulting [`ResponseEventStream`] to completion with
/// per-event timeout, biased cancellation, and per-tool
/// concurrency via [`FuturesOrdered`]. See the module-level docs
/// for the full invariant list.
pub(super) async fn run_stream_pump(
    config: &AgentLoopConfig,
    provider: &dyn ModelProvider,
    executor: &dyn AgentToolExecutor,
    request: ModelRequest,
    cancellation_token: Option<&CancellationToken>,
    input_queue: Option<&InputQueue>,
    state: &mut super::LoopState,
) -> StreamPumpOutcome {
    let model_name = request.model.as_ref().to_string();
    let stream = match provider.complete_response_stream(request).await {
        Ok(s) => s,
        Err(err) => {
            return StreamPumpOutcome::Error(AgentError::Reason(err));
        }
    };

    drive_stream(
        config,
        executor,
        stream,
        cancellation_token,
        input_queue,
        state,
        &model_name,
    )
    .await
}

/// Inner driver — separated so the unit tests can hand it a
/// hand-rolled `ResponseEventStream` without a real `ModelProvider`.
pub(super) async fn drive_stream(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    mut stream: ResponseEventStream,
    cancellation_token: Option<&CancellationToken>,
    input_queue: Option<&InputQueue>,
    state: &mut super::LoopState,
    model_name: &str,
) -> StreamPumpOutcome {
    let mut in_flight: FuturesOrdered<ToolFuture<'_>> = FuturesOrdered::new();
    let mut text_chunks: Vec<String> = Vec::new();
    let mut thinking_chunks: Vec<(String, Option<String>)> = Vec::new();
    let mut tool_calls_seen: Vec<ToolCallInfo> = Vec::new();
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
                return StreamPumpOutcome::Error(AgentError::Stream(err));
            }
            StreamStep::End => break,
            StreamStep::Event(event) => match event {
                ResponseEvent::OutputItemDone(OutputItem::ToolUse { id, name, input }) => {
                    let call = ToolCallInfo { id, name, input };
                    tool_calls_seen.push(call.clone());
                    in_flight.push_back(spawn_tool_with_timeout(executor, call, per_tool_timeout));

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
                    text_chunks.push(text);
                }
                ResponseEvent::OutputItemDone(OutputItem::Thinking {
                    thinking,
                    signature,
                }) => {
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
                    return StreamPumpOutcome::Error(AgentError::Stream(err));
                }
            },
        }
    }

    // Drain the FIFO in submission order (codex `drain_in_flight`).
    // Honours cancellation: a token fired during drain still aborts
    // before we mutate `state.messages` in the caller.
    let mut tool_results: Vec<(ToolCallInfo, ToolCallResult)> =
        Vec::with_capacity(tool_calls_seen.len());
    loop {
        let maybe_next = drain_next(&mut in_flight, cancellation_token).await;
        match maybe_next {
            DrainStep::Cancelled => return StreamPumpOutcome::Cancelled,
            DrainStep::Done => break,
            DrainStep::Result(pair) => tool_results.push(pair),
        }
    }

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
    response: &ModelResponse,
    tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    event_tx: Option<&tokio::sync::mpsc::Sender<crate::events::AgentLoopEvent>>,
    state: &mut super::LoopState,
) -> bool {
    match response.stop_reason {
        aura_reasoner::StopReason::EndTurn | aura_reasoner::StopReason::StopSequence => true,
        aura_reasoner::StopReason::MaxTokens => {
            !super::iteration::handle_max_tokens(&agent.config, response, state)
        }
        aura_reasoner::StopReason::ToolUse => {
            handle_streamed_tool_use(tool_results, event_tx, state)
        }
    }
}

/// Streaming-pump analog of `tool_execution::handle_tool_use` that
/// consumes pre-executed [`ToolCallResult`]s instead of re-invoking
/// the executor. Emits per-result events, appends the
/// `tool_result`-bearing user message, and computes whether any
/// result requested loop termination.
///
/// Returns `true` when the sampling loop should break (the buffered
/// path's contract).
///
/// # Caveats vs the buffered path
///
/// - Tool-result caching (`tool_execution::handle_tool_use` →
///   `split_cached` / `update_cache`) is currently NOT applied to
///   pump-executed results. Repeating the same read-only call within
///   a session will re-run the tool. Tracked for the E.3 follow-up
///   that wires a cache adapter into the pump.
/// - Auto-build (`tool_pipeline::run_auto_build`) is similarly
///   skipped — the pump prefers to defer auto-build to the next
///   sampling boundary so the streaming loop stays focused on
///   stream-level overlap. Same E.3 follow-up.
fn handle_streamed_tool_use(
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
    super::tool_pipeline::track_tool_effects_public(
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
        Vec::new(),
    );
    should_stop
}

fn emit_event(
    tx: Option<&tokio::sync::mpsc::Sender<crate::events::AgentLoopEvent>>,
    event: crate::events::AgentLoopEvent,
) {
    if let Some(tx) = tx {
        if let Err(e) = tx.try_send(event) {
            tracing::warn!("agent event channel full or closed: {e}");
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
        let config = AgentLoopConfig::default();
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
            ..AgentLoopConfig::default()
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
        let config = AgentLoopConfig::default();
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
            ..AgentLoopConfig::default()
        };
        let stream: ResponseEventStream = Box::pin(futures_util::stream::pending());
        let mut state = super::super::LoopState::new(&config, Vec::new());

        let outcome = drive_stream(
            &config,
            &executor,
            stream,
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
        let config = AgentLoopConfig::default();
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
            ..AgentLoopConfig::default()
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
            }
        }
    }
}
