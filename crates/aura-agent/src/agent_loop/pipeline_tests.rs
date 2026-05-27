//! Pipeline integration tests: `tool_use` → cache-split → execute → result → message.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aura_reasoner::{
    ContentBlock, Message, MockProvider, MockResponse, ModelProvider, ModelRequest, ModelResponse,
    ReasonerError, StopReason, StreamContentType, StreamEvent, StreamEventStream, ToolDefinition,
    ToolResultContent, Usage,
};
use futures_util::stream;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use super::{AgentLoop, AgentLoopConfig};
use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, AutoBuildResult, ToolCallInfo, ToolCallResult};

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

struct SuccessExecutor;

#[async_trait::async_trait]
impl AgentToolExecutor for SuccessExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "ok"))
            .collect()
    }
}

struct CountingExecutor {
    call_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentToolExecutor for CountingExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        self.call_count
            .fetch_add(tool_calls.len(), Ordering::SeqCst);
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "result"))
            .collect()
    }
}

struct BuildCheckExecutor;

#[async_trait::async_trait]
impl AgentToolExecutor for BuildCheckExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "ok"))
            .collect()
    }

    async fn auto_build_check(&self) -> Option<AutoBuildResult> {
        Some(AutoBuildResult {
            success: false,
            output: "error[E0308]: mismatched types".to_string(),
            error_count: 1,
        })
    }
}

// ---------------------------------------------------------------------------
// StreamingMockProvider — emits proper tool-use stream events so the
// StreamAccumulator reconstructs ToolUse content blocks.
// ---------------------------------------------------------------------------

struct StreamingMockProvider {
    inner: MockProvider,
}

#[async_trait::async_trait]
impl ModelProvider for StreamingMockProvider {
    fn name(&self) -> &'static str {
        "streaming_mock"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let response = self.inner.complete(request).await?;
        let mut events: Vec<Result<StreamEvent, ReasonerError>> = Vec::new();

        events.push(Ok(StreamEvent::MessageStart {
            message_id: "msg_test".to_string(),
            model: "mock-model".to_string(),
            input_tokens: Some(response.usage.input_tokens),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }));

        for (idx, block) in response.message.content.iter().enumerate() {
            let index = idx as u32;
            match block {
                ContentBlock::Text { text } => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index,
                        content_type: StreamContentType::Text,
                    }));
                    events.push(Ok(StreamEvent::TextDelta { text: text.clone() }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index }));
                }
                ContentBlock::ToolUse { id, name, input } => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index,
                        content_type: StreamContentType::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                        },
                    }));
                    let json = serde_json::to_string(input).unwrap_or_default();
                    events.push(Ok(StreamEvent::InputJsonDelta { partial_json: json }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index }));
                }
                ContentBlock::Thinking { thinking, .. } => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index,
                        content_type: StreamContentType::Thinking,
                    }));
                    events.push(Ok(StreamEvent::ThinkingDelta {
                        thinking: thinking.clone(),
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index }));
                }
                _ => {}
            }
        }

        events.push(Ok(StreamEvent::MessageDelta {
            stop_reason: Some(response.stop_reason),
            output_tokens: response.usage.output_tokens,
        }));
        events.push(Ok(StreamEvent::MessageStop));

        Ok(Box::pin(stream::iter(events)))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_config() -> AgentLoopConfig {
    AgentLoopConfig {
        system_prompt: "pipeline test agent".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    }
}

fn read_file_tool() -> ToolDefinition {
    ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )
}

fn write_file_tool() -> ToolDefinition {
    ToolDefinition::new(
        "write_file",
        "Write a file",
        serde_json::json!({"type": "object"}),
    )
}

async fn collect_events(mut rx: mpsc::Receiver<AgentLoopEvent>) -> Vec<AgentLoopEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_full_tool_execution_flow() {
    let provider = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "tool_1",
            "read_file",
            serde_json::json!({"path": "test.txt"}),
        ))
        .with_response(MockResponse::text("done"));

    let executor = SuccessExecutor;
    let agent = AgentLoop::new(default_config());
    let messages = vec![Message::user("Read test.txt")];
    let tools = vec![read_file_tool()];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);

    let has_tool_result = result.messages.iter().any(|msg| {
        msg.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    });
    assert!(has_tool_result, "Messages must contain a tool_result block");
    assert!(
        result.total_text.contains("done"),
        "Final text must contain 'done'"
    );
}

#[tokio::test]
async fn pipeline_write_success_clears_cache() {
    let counter = Arc::new(AtomicUsize::new(0));
    let executor = CountingExecutor {
        call_count: Arc::clone(&counter),
    };

    // Phase 1B: a successful write only invalidates cache entries
    // whose path overlaps the written one. To keep the original
    // "write clears the read cache, so the second read_file
    // re-executes" assertion, target the write at the same path
    // the read cached. The path-isolation half of the new contract
    // is covered by `write_invalidates_only_overlapping_path` in
    // `tool_execution_tests`.
    let provider = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "t1",
            "read_file",
            serde_json::json!({"path": "test.txt"}),
        ))
        .with_response(MockResponse::tool_use(
            "t2",
            "write_file",
            serde_json::json!({"path": "test.txt", "content": "fn main() {}"}),
        ))
        .with_response(MockResponse::tool_use(
            "t3",
            "read_file",
            serde_json::json!({"path": "test.txt"}),
        ))
        .with_response(MockResponse::text("done"));

    let agent = AgentLoop::new(default_config());
    let messages = vec![Message::user("read, write, read again")];
    let tools = vec![read_file_tool(), write_file_tool()];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 4);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "read_file executed twice (cache cleared by write to same path) + one write = 3 total"
    );
}

#[tokio::test]
async fn pipeline_every_tool_emits_result_event() {
    let inner = MockProvider::new()
        .with_response(MockResponse {
            stop_reason: StopReason::ToolUse,
            content: vec![
                ContentBlock::tool_use("t1", "read_file", serde_json::json!({"path": "a.txt"})),
                ContentBlock::tool_use("t2", "read_file", serde_json::json!({"path": "b.txt"})),
            ],
            usage: Usage::new(100, 50),
        })
        .with_response(MockResponse::text("done"));

    let provider = StreamingMockProvider { inner };
    let executor = SuccessExecutor;
    let agent = AgentLoop::new(default_config());

    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("read two files")];
    let tools = vec![read_file_tool()];

    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);

    let events = collect_events(rx).await;
    let tool_result_count = events
        .iter()
        .filter(|e| matches!(e, AgentLoopEvent::ToolResult { .. }))
        .count();

    assert_eq!(
        tool_result_count, 2,
        "Each tool execution must emit a ToolResult event"
    );
}

/// Phase 6 of agent-stuck-and-reset: while a long-running tool is
/// in flight, [`super::tool_pipeline::spawn_tool_heartbeat`] must
/// emit periodic [`AgentLoopEvent::Progress`] frames with
/// `stage: "tool_running"` so aura-os's sliding-idle watchdog (and
/// the client-side stuck-stream watchdog) see forward motion.
///
/// Drives the spawn helper directly with a 50ms cadence so the test
/// stays under a second; full-loop coverage of the same path lives
/// in [`pipeline_heartbeat_fires_during_long_tool_call`].
#[tokio::test]
async fn spawn_tool_heartbeat_emits_periodic_progress_events() {
    use std::time::Duration;
    use tokio::time::timeout;

    use super::tool_pipeline::spawn_tool_heartbeat;

    let (tx, mut rx) = mpsc::channel::<AgentLoopEvent>(16);
    let to_execute = vec![ToolCallInfo {
        id: "call_1".to_string(),
        name: "slow_tool".to_string(),
        input: serde_json::json!({}),
    }];

    let guard = spawn_tool_heartbeat(Some(&tx), &to_execute, Duration::from_millis(50));

    let mut tool_running_events = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while tool_running_events < 2 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(150), rx.recv()).await {
            Ok(Some(AgentLoopEvent::Progress {
                stage,
                tool_name,
                elapsed_ms,
                ..
            })) => {
                if stage == "tool_running" {
                    assert_eq!(tool_name.as_deref(), Some("slow_tool"));
                    assert!(
                        elapsed_ms.unwrap_or(0) >= 50,
                        "elapsed_ms must reflect at least one full interval"
                    );
                    tool_running_events += 1;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }

    drop(guard);
    assert!(
        tool_running_events >= 2,
        "expected at least 2 tool_running heartbeats inside the wait \
         window, got {tool_running_events}"
    );
}

/// Drop semantics: once the tool batch returns and the
/// [`super::tool_pipeline::HeartbeatGuard`] is dropped, no further
/// `tool_running` heartbeats may land on the channel. Guards a
/// regression where the heartbeat task could outlive the tool call
/// and keep emitting frames against a closed turn (which would race
/// with `AssistantMessageEnd` ordering on the wire).
#[tokio::test]
async fn spawn_tool_heartbeat_stops_after_guard_drops() {
    use std::time::Duration;
    use tokio::time::timeout;

    use super::tool_pipeline::spawn_tool_heartbeat;

    let (tx, mut rx) = mpsc::channel::<AgentLoopEvent>(16);
    let to_execute = vec![ToolCallInfo {
        id: "call_1".to_string(),
        name: "slow_tool".to_string(),
        input: serde_json::json!({}),
    }];

    {
        let _guard = spawn_tool_heartbeat(Some(&tx), &to_execute, Duration::from_millis(20));
        let _ = timeout(Duration::from_millis(80), rx.recv()).await;
    }
    while let Ok(Some(_)) = timeout(Duration::from_millis(20), rx.recv()).await {}

    let post_drop = timeout(Duration::from_millis(120), rx.recv()).await;
    let none_arrived = match post_drop {
        Err(_) => true,
        Ok(None) => true,
        Ok(Some(AgentLoopEvent::Progress { .. })) => false,
        Ok(Some(_)) => true,
    };
    assert!(
        none_arrived,
        "heartbeat task must abort when its guard drops; received a Progress \
         frame after drop"
    );
}

/// Heartbeat env knob clamping (now via `aura_config`): zero is bumped
/// to the floor, gigantic values clamp to the ceiling. The env-var
/// name (`AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS`) is shared with the
/// aura-os watchdog; the boundary tests assert no caller outside
/// `aura-config` parses it directly.
#[test]
fn tool_heartbeat_interval_clamps_via_aura_config() {
    use std::time::Duration;

    use super::tool_pipeline::tool_heartbeat_interval;

    fn install(secs: u64) -> aura_config::ConfigGuard {
        let mut cfg = aura_config::current();
        cfg.agent.tools.heartbeat_interval = Duration::from_secs(secs);
        aura_config::install_for_test(cfg)
    }

    {
        let _g = install(10);
        assert_eq!(tool_heartbeat_interval(), Duration::from_secs(10));
    }
    {
        let _g = install(aura_config::MIN_TOOL_HEARTBEAT_INTERVAL_SECS);
        assert_eq!(
            tool_heartbeat_interval(),
            Duration::from_secs(aura_config::MIN_TOOL_HEARTBEAT_INTERVAL_SECS)
        );
    }
    {
        let _g = install(aura_config::MAX_TOOL_HEARTBEAT_INTERVAL_SECS);
        assert_eq!(
            tool_heartbeat_interval(),
            Duration::from_secs(aura_config::MAX_TOOL_HEARTBEAT_INTERVAL_SECS)
        );
    }
}

/// Executor that signals start via a `Notify`, then awaits an
/// `unblock` signal before returning. Lets the test deterministically
/// observe "tool execution has started" so it can fire the
/// cancellation token at exactly the right moment.
struct BlockingExecutor {
    started: Arc<Notify>,
    unblock: Arc<Notify>,
}

#[async_trait::async_trait]
impl AgentToolExecutor for BlockingExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        self.started.notify_one();
        self.unblock.notified().await;
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "executor returned despite cancellation"))
            .collect()
    }
}

/// Stop pressed while a tool is in flight: the agent loop must observe
/// cancellation via the `tokio::select!` in `process_tool_results`,
/// synthesize a `[CANCELLED]` tool_result with `stop_loop: true`, and
/// break the loop on the first iteration's `check_termination_conditions`
/// — not after the long-running executor naturally returns. Pinned
/// because the pre-fix behaviour was "warn-and-continue": stop ran on
/// the harness side but `executor.execute(...).await` had no cancel
/// observation, so the loop kept the agent alive (and any spawned
/// child processes) for as long as the tool took to finish.
#[tokio::test]
async fn pipeline_cancellation_mid_tool_execution_aborts_loop() {
    use std::time::Duration;
    use tokio::time::timeout;

    let provider = MockProvider::new().with_response(MockResponse::tool_use(
        "tc_slow",
        "read_file",
        serde_json::json!({"path": "huge.txt"}),
    ));

    let started = Arc::new(Notify::new());
    let unblock = Arc::new(Notify::new());
    let executor = BlockingExecutor {
        started: Arc::clone(&started),
        unblock: Arc::clone(&unblock),
    };

    let agent = AgentLoop::new(default_config());
    let messages = vec![Message::user("read huge.txt")];
    let tools = vec![read_file_tool()];
    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();

    let run = tokio::spawn(async move {
        agent
            .run_with_events(
                &provider,
                &executor,
                messages,
                tools,
                None,
                Some(cancel_for_run),
            )
            .await
    });

    timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("executor.execute must be reached before cancellation fires");

    cancel.cancel();

    let result = timeout(Duration::from_secs(2), run)
        .await
        .expect("loop must observe cancellation and terminate without unblock")
        .expect("loop task must not panic")
        .expect("loop must return Ok even when cancelled mid-tool");

    let cancelled_tool_result = result.messages.iter().any(|msg| {
        msg.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(text),
                    ..
                } if text.contains("[CANCELLED]")
            )
        })
    });
    assert!(
        cancelled_tool_result,
        "mid-tool cancellation must synthesize a [CANCELLED] tool_result so the \
         Anthropic tool_use<->tool_result adjacency contract stays intact"
    );
    assert_eq!(
        result.iterations, 1,
        "the loop must break on the same iteration as the cancellation; pre-fix \
         this iterated until the executor naturally returned"
    );
}

/// Stop pressed between `call_model` returning and tool dispatch: the
/// agent loop's post-streaming cancellation check must short-circuit
/// `dispatch_stop_reason` so no fresh tool batch starts after the user
/// pressed Stop. Pre-fix the loop happily dispatched the new batch and
/// only noticed cancellation at the top of the NEXT iteration.
#[tokio::test]
async fn pipeline_cancellation_after_call_model_skips_tool_dispatch() {
    use std::sync::atomic::AtomicBool;

    struct PoisonExecutor {
        called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl AgentToolExecutor for PoisonExecutor {
        async fn execute(&self, _tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
            self.called.store(true, Ordering::SeqCst);
            panic!("executor.execute must NOT run when cancellation is observed pre-dispatch");
        }
    }

    /// Cancels the supplied token the first time `complete_streaming`
    /// is invoked. Simulates the "Stop pressed during the model's
    /// stream" race that motivated the post-`call_model` cancellation
    /// check in `run_inner`.
    struct CancellingProvider {
        inner: StreamingMockProvider,
        cancel: CancellationToken,
    }

    #[async_trait::async_trait]
    impl ModelProvider for CancellingProvider {
        fn name(&self) -> &'static str {
            "cancelling_mock"
        }

        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
            self.inner.complete(request).await
        }

        async fn complete_streaming(
            &self,
            request: ModelRequest,
        ) -> Result<StreamEventStream, ReasonerError> {
            let stream = self.inner.complete_streaming(request).await?;
            self.cancel.cancel();
            Ok(stream)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    let inner = MockProvider::new().with_response(MockResponse::tool_use(
        "tc_after",
        "read_file",
        serde_json::json!({"path": "a.txt"}),
    ));
    let cancel = CancellationToken::new();
    let provider = CancellingProvider {
        inner: StreamingMockProvider { inner },
        cancel: cancel.clone(),
    };

    let called = Arc::new(AtomicBool::new(false));
    let executor = PoisonExecutor {
        called: Arc::clone(&called),
    };

    let agent = AgentLoop::new(default_config());
    let messages = vec![Message::user("read a.txt")];
    let tools = vec![read_file_tool()];
    // `event_tx = Some(_)` is required to take the streaming path
    // through `complete_with_streaming` — the non-streaming
    // `provider.complete(...)` branch never observes the cancellation
    // triggered by the provider during `complete_streaming`.
    let (event_tx, _event_rx) = mpsc::channel(1024);

    let result = agent
        .run_with_events(
            &provider,
            &executor,
            messages,
            tools,
            Some(event_tx),
            Some(cancel),
        )
        .await
        .expect("loop must return Ok when cancellation is observed post-streaming");

    assert!(
        !called.load(Ordering::SeqCst),
        "executor.execute must not run after cancellation observed post-streaming"
    );
    // Either the streaming layer's own cancellation check (`DrainOutcome::Cancelled`)
    // or `run_inner`'s post-`call_model` check (the new defense-in-depth guard
    // added alongside this test) is allowed to win — both result in "tools
    // never dispatched after Stop". `iterations == 0` happens when streaming
    // wins (cancellation breaks the drain loop before `accumulate_response` /
    // `state.result.iterations = iteration + 1` run); `iterations == 1` happens
    // when streaming completes synchronously and the post-`call_model` check
    // breaks the loop after the iteration counter advances.
    assert!(
        result.iterations <= 1,
        "the loop must break on or before the first iteration; got {iters}",
        iters = result.iterations
    );
}

#[tokio::test]
async fn pipeline_auto_build_after_write() {
    let provider = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "t1",
            "write_file",
            serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
        ))
        .with_response(MockResponse::text("done"));

    let executor = BuildCheckExecutor;
    let agent = AgentLoop::new(default_config());
    let messages = vec![Message::user("write src/main.rs")];
    let tools = vec![write_file_tool()];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);

    let has_build_failure = result.messages.iter().any(|msg| {
        msg.content.iter().any(|block| {
            if let ContentBlock::Text { text } = block {
                text.contains("Build check failed") && text.contains("error[E0308]")
            } else {
                false
            }
        })
    });
    assert!(
        has_build_failure,
        "Messages must contain the build failure output"
    );
}
