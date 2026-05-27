//! Pump-scope unit tests.
//!
//! Note: the bulk of the E.3 mandatory tests live in
//! `crate::agent_loop::stream_pump_tests` so they can use scripted
//! fake providers and `start_paused = true` time control without
//! pulling the full sampling driver into scope.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_reasoner::{
    ContentBlock, Message, ModelResponse, OutputItem, ProviderTrace, ResponseEvent,
    ResponseEventStream, Role, StopReason, Usage,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::events::AgentLoopEvent;
use crate::session::input_queue::InputQueue;
use crate::session::SessionId;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};
use crate::AgentError;

use super::driver::drive_stream;
use super::AgentLoopConfig;
use super::StreamPumpOutcome;

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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());

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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());

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
        .expect("pump test push: queue is drop-on-cancel only");
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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());

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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());

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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());
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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());

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
            assert!(!tool_results[0].1.is_error);
            assert!(tool_results[1].1.is_error, "hung tool should error");
            assert!(
                tool_results[1].1.content.contains("timed out"),
                "hung tool should mention timeout"
            );
            assert!(
                !tool_results[2].1.is_error,
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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());
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
    let cache_key = aura_config::tool_result_cache_key("read_file", &cached_input);
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());
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
    let mut state = super::super::LoopState::new_for_tests(&config, Vec::new());
    let agent = super::super::AgentLoop::new(config.clone());

    // Drive only the post-stream dispatch path — the pre-stream
    // pump already has its own coverage, and the auto-build
    // wiring lives in `handle_streamed_tool_use`.
    let _should_break = super::dispatch_streamed_response(
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
