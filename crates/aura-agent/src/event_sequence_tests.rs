//! Event-sequence assertion tests for the agent loop.
//!
//! Verifies that `AgentLoop::run_with_events` emits the correct
//! `AgentLoopEvent` variants in the expected order.

use aura_model_reasoner::{
    ContentBlock, Message, MockProvider, MockResponse, ModelProvider, ModelRequest, ModelResponse,
    ReasonerError, StreamContentType, StreamEvent, StreamEventStream, ToolDefinition,
};
use futures_util::stream;
use tokio::sync::mpsc;

use crate::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

// ============================================================================
// Test Helpers
// ============================================================================

struct MockExecutor {
    results: Vec<ToolCallResult>,
}

#[async_trait::async_trait]
impl AgentToolExecutor for MockExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        tool_calls
            .iter()
            .zip(self.results.iter())
            .map(|(tc, r)| ToolCallResult {
                tool_use_id: tc.id.clone(),
                ..r.clone()
            })
            .collect()
    }
}

/// A mock provider that properly streams tool use events.
///
/// The default `ModelProvider::complete_streaming` fallback only emits
/// `TextDelta` events, which causes the `StreamAccumulator` to lose
/// tool use blocks. This wrapper emits proper `ContentBlockStart(ToolUse)`
/// and `InputJsonDelta` events so streaming tool use works end-to-end.
struct StreamingMockProvider {
    inner: MockProvider,
}

impl StreamingMockProvider {
    fn new(inner: MockProvider) -> Self {
        Self { inner }
    }
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

/// Drain all events from the channel after the sender has been dropped.
async fn collect_events(mut rx: mpsc::Receiver<AgentLoopEvent>) -> Vec<AgentLoopEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn text_response_emits_text_delta_then_iteration_complete() {
    let provider = StreamingMockProvider::new(MockProvider::simple_response("Hello, world!"));
    let executor = MockExecutor { results: vec![] };
    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);

    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hi")];
    let tools = vec![];

    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);

    let events = collect_events(rx).await;

    let text_delta_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::TextDelta(_)));
    let iter_complete_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::IterationComplete { .. }));

    assert!(text_delta_pos.is_some(), "Should have TextDelta event");
    assert!(
        iter_complete_pos.is_some(),
        "Should have IterationComplete event"
    );
    assert!(
        text_delta_pos.unwrap() < iter_complete_pos.unwrap(),
        "TextDelta should come before IterationComplete"
    );

    if let AgentLoopEvent::TextDelta(text) = &events[text_delta_pos.unwrap()] {
        assert_eq!(text, "Hello, world!");
    } else {
        panic!("Expected TextDelta");
    }

    if let AgentLoopEvent::IterationComplete { iteration, .. } = &events[iter_complete_pos.unwrap()]
    {
        assert_eq!(*iteration, 0);
    } else {
        panic!("Expected IterationComplete");
    }
}

#[tokio::test]
async fn tool_use_emits_tool_start_input_snapshot_then_result() {
    let inner = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "tool_1",
            "read_file",
            serde_json::json!({"path": "test.txt"}),
        ))
        .with_response(MockResponse::text("All done!"));
    let provider = StreamingMockProvider::new(inner);

    let executor = MockExecutor {
        results: vec![ToolCallResult::success("placeholder", "file contents here")],
    };
    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);

    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("Read test.txt")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);

    let events = collect_events(rx).await;

    let tool_start_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::ToolStart { .. }));
    let tool_input_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::ToolInputSnapshot { .. }));
    let tool_result_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::ToolResult { .. }));

    assert!(tool_start_pos.is_some(), "Should have ToolStart event");
    assert!(
        tool_input_pos.is_some(),
        "Should have ToolInputSnapshot event"
    );
    assert!(tool_result_pos.is_some(), "Should have ToolResult event");

    assert!(
        tool_start_pos.unwrap() < tool_input_pos.unwrap(),
        "ToolStart should come before ToolInputSnapshot"
    );
    assert!(
        tool_input_pos.unwrap() < tool_result_pos.unwrap(),
        "ToolInputSnapshot should come before ToolResult"
    );

    if let AgentLoopEvent::ToolStart { id, name } = &events[tool_start_pos.unwrap()] {
        assert_eq!(id, "tool_1");
        assert_eq!(name, "read_file");
    } else {
        panic!("Expected ToolStart");
    }

    if let AgentLoopEvent::ToolResult {
        tool_use_id,
        tool_name,
        content,
        is_error,
        ..
    } = &events[tool_result_pos.unwrap()]
    {
        assert_eq!(tool_use_id, "tool_1");
        assert_eq!(tool_name, "read_file");
        assert_eq!(content, "file contents here");
        assert!(!is_error);
    } else {
        panic!("Expected ToolResult");
    }
}

#[tokio::test]
async fn budget_warning_emits_warning_event() {
    // max_iterations = 3 → after iteration 0, utilization = 1/3 ≈ 0.33 > 0.30,
    // so the 30% budget warning fires.
    let inner = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "t0",
            "read_file",
            serde_json::json!({"path": "a.txt"}),
        ))
        .with_response(MockResponse::text("Done"));
    let provider = StreamingMockProvider::new(inner);

    let executor = MockExecutor {
        results: vec![ToolCallResult::success("placeholder", "ok")],
    };

    let config = AgentLoopConfig {
        max_iterations: 3,
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);

    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("go")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let _result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    let events = collect_events(rx).await;

    let warnings: Vec<&str> = events
        .iter()
        .filter_map(|e| {
            if let AgentLoopEvent::Warning(msg) = e {
                Some(msg.as_str())
            } else {
                None
            }
        })
        .collect();

    assert!(
        warnings.iter().any(|w| w.contains("30%")),
        "Should have a 30% budget warning, got warnings: {warnings:?}"
    );
}

#[tokio::test]
async fn llm_error_emits_error_event() {
    let provider = MockProvider::new().with_failure();
    let executor = MockExecutor { results: vec![] };

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);

    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hi")];
    let tools = vec![];

    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert!(result.llm_error.is_some());

    let events = collect_events(rx).await;

    let errors: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| {
            if let AgentLoopEvent::Error {
                code, recoverable, ..
            } = e
            {
                Some((code.as_str(), *recoverable))
            } else {
                None
            }
        })
        .collect();

    assert!(
        errors
            .iter()
            .any(|(code, recoverable)| *code == "llm_error" && !recoverable),
        "Should have an llm_error event, got errors: {errors:?}"
    );
}
