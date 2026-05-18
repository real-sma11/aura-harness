//! Core agent loop tests: config defaults, simple runs, error handling, max-tokens.

use aura_reasoner::{
    ContentBlock, Message, MockProvider, MockResponse, ModelProvider, ModelRequest,
    ModelRequestKind, ModelResponse, ProviderTrace, ReasonerError, StopReason, ToolDefinition,
    Usage,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

use super::{AgentLoop, AgentLoopConfig};
use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

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

struct OverflowThenSuccessProvider {
    failures_before_success: usize,
    call_count: AtomicUsize,
    seen_max_tokens: Mutex<Vec<u32>>,
}

struct SummaryThenFinalProvider {
    call_count: AtomicUsize,
    request_kinds: Mutex<Vec<Option<ModelRequestKind>>>,
}

#[async_trait::async_trait]
impl ModelProvider for SummaryThenFinalProvider {
    fn name(&self) -> &'static str {
        "summary-then-final"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        self.request_kinds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.metadata.kind);
        let call = self.call_count.fetch_add(1, Ordering::SeqCst);
        let text = if call == 0 {
            "Summary: earlier turns explored large implementation details."
        } else {
            "Done after summary compaction."
        };
        Ok(ModelResponse::new(
            StopReason::EndTurn,
            Message::assistant(text),
            Usage::new(100, 20),
            ProviderTrace::new("summary-mock", 0),
        ))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl ModelProvider for OverflowThenSuccessProvider {
    fn name(&self) -> &'static str {
        "overflow-then-success"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        self.seen_max_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.max_tokens.get());
        let call = self.call_count.fetch_add(1, Ordering::SeqCst);
        if call < self.failures_before_success {
            return Err(ReasonerError::Api {
                status: 400,
                message: "input length and max_tokens exceed context limit".to_string(),
            });
        }

        Ok(ModelResponse::new(
            StopReason::EndTurn,
            Message::assistant("Recovered after overflow."),
            Usage::new(400, 120),
            ProviderTrace::new("overflow-mock", 0),
        ))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[test]
fn test_agent_loop_config_defaults() {
    let config = AgentLoopConfig::default();
    // Default is `usize::MAX` (unlimited). Termination is driven by
    // `EndTurn`, the credit budget, or cooperative cancellation. The
    // 25-iteration cap was raised because it silently truncated
    // long-running batch workflows (e.g. multi-`create_task`
    // extraction) with `stop_reason: "cancelled"`. See
    // `constants::MAX_ITERATIONS`.
    assert_eq!(config.max_iterations, usize::MAX);
    assert_eq!(config.exploration_allowance, 12);
    assert_eq!(config.auto_build_cooldown, 2);
    assert_eq!(config.thinking_taper_after, 2);
    assert!((config.thinking_taper_factor - 0.6).abs() < f64::EPSILON);
    // Floor raised from 1024 → 6144 to fit a full-size tool-call JSON
    // (harness observed `edit_file` truncations at ~2.5 KB / ~1000
    // tokens plus preceding reasoning). See `constants::THINKING_MIN_BUDGET`.
    assert_eq!(config.thinking_min_budget, 6144);
}

#[tokio::test]
async fn test_agent_loop_simple_run() {
    let config = AgentLoopConfig::default();
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::simple_response("Hello!");
    let messages = vec![Message::user("hello")];
    let tools = vec![];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();
    assert_eq!(result.iterations, 1);
    assert!(result.total_text.contains("Hello!"));
    assert!(result.total_input_tokens > 0);
}

#[tokio::test]
async fn test_agent_loop_full_integration() {
    let executor = MockExecutor {
        results: vec![ToolCallResult::success("placeholder", "file contents here")],
    };

    let provider = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "tool_1",
            "read_file",
            serde_json::json!({"path": "test.txt"}),
        ))
        .with_response(MockResponse::text("All done!"));

    let config = AgentLoopConfig {
        system_prompt: "You are a test agent".to_string(),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("Read test.txt")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);
    assert!(result.total_text.contains("All done!"));
    assert!(result.total_input_tokens > 0);
    assert!(result.total_output_tokens > 0);
    assert!(!result.insufficient_credits);
    assert!(result.llm_error.is_none());
}

#[tokio::test]
async fn test_agent_loop_402_insufficient_credits() {
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::new().with_failure();

    let config = AgentLoopConfig::default();
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("hello")];
    let tools = vec![];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();
    assert!(result.llm_error.is_some());
}

#[tokio::test]
async fn test_max_tokens_with_pending_tools_injects_errors() {
    let executor = MockExecutor { results: vec![] };

    let provider = MockProvider::new()
        .with_response(
            MockResponse::tool_use(
                "tool_1",
                "read_file",
                serde_json::json!({"path": "big_file.txt"}),
            )
            .with_stop_reason(StopReason::MaxTokens),
        )
        .with_response(MockResponse::text("Recovered after truncation."));

    let config = AgentLoopConfig {
        system_prompt: "Test agent".to_string(),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("Read big_file.txt")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 2,
        "Loop should continue after MaxTokens with pending tools"
    );
    assert!(result.total_text.contains("Recovered after truncation."));

    let has_error_tool_result = result.messages.iter().any(|msg| {
        msg.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
    });
    assert!(
        has_error_tool_result,
        "Should have injected an error tool result"
    );
}

#[tokio::test]
async fn test_max_tokens_without_tools_breaks() {
    let executor = MockExecutor { results: vec![] };

    let provider = MockProvider::new()
        .with_response(MockResponse::text("Truncated text").with_stop_reason(StopReason::MaxTokens))
        .with_response(MockResponse::text("Should not reach this"));

    let config = AgentLoopConfig {
        system_prompt: "Test agent".to_string(),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("hello")];
    let tools = vec![];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 1,
        "Loop should break on MaxTokens with no pending tools"
    );
    assert!(result.total_text.contains("Truncated text"));
    assert!(!result.total_text.contains("Should not reach this"));
}

#[test]
fn test_tool_call_result_defaults() {
    let result = ToolCallResult::success("id", "content");
    assert!(!result.is_error);
    assert!(!result.stop_loop);

    let err = ToolCallResult::error("id", "error");
    assert!(err.is_error);
    assert!(!err.stop_loop);
}

#[tokio::test]
async fn test_compaction_uses_api_input_tokens() {
    let executor = MockExecutor {
        results: vec![ToolCallResult::success("placeholder", "ok")],
    };

    let high_usage_tool = MockResponse {
        stop_reason: StopReason::ToolUse,
        content: vec![ContentBlock::tool_use(
            "tool_1",
            "read_file",
            serde_json::json!({"path": "big.txt"}),
        )],
        usage: Usage::new(180_000, 50),
    };
    let final_resp = MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("Done")],
        usage: Usage::new(185_000, 50),
    };

    let provider = MockProvider::new()
        .with_response(high_usage_tool)
        .with_response(final_resp);

    let config = AgentLoopConfig {
        max_context_tokens: Some(200_000),
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("go")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);
    assert_eq!(result.total_input_tokens, 180_000 + 185_000);
    assert_eq!(result.estimated_context_tokens, 185_050);
}

#[tokio::test]
async fn test_context_estimate_includes_cache_tokens() {
    let config = AgentLoopConfig::default();
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::new().with_response(MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("Hello!")],
        usage: Usage::new(100_000, 2_000).with_cache(Some(5_000), Some(7_000)),
    });
    let messages = vec![Message::user("hello")];
    let tools = vec![];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert_eq!(result.estimated_context_tokens, 114_000);
}

#[tokio::test]
async fn test_prompt_overflow_retries_after_compaction() {
    let config = AgentLoopConfig {
        max_context_tokens: Some(20_000),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = OverflowThenSuccessProvider {
        failures_before_success: 1,
        call_count: AtomicUsize::new(0),
        seen_max_tokens: Mutex::new(Vec::new()),
    };
    let large = "history ".repeat(1_200);
    let messages = vec![
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user("Please continue"),
    ];

    let result = agent
        .run(&provider, &executor, messages, vec![])
        .await
        .unwrap();

    assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
    assert!(result.llm_error.is_none());
    assert_eq!(result.iterations, 1);
    assert!(result.total_text.contains("Recovered after overflow."));
}

#[tokio::test]
async fn test_prompt_overflow_fails_fast_when_compaction_cannot_help() {
    let config = AgentLoopConfig {
        max_context_tokens: Some(20_000),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = OverflowThenSuccessProvider {
        failures_before_success: usize::MAX,
        call_count: AtomicUsize::new(0),
        seen_max_tokens: Mutex::new(Vec::new()),
    };
    let messages = vec![Message::user("hello")];

    let result = agent
        .run(&provider, &executor, messages, vec![])
        .await
        .unwrap();

    assert_eq!(provider.call_count.load(Ordering::SeqCst), 1);
    assert!(result.total_text.is_empty());
    assert!(result.llm_error.is_some());
}

#[tokio::test]
async fn test_prompt_overflow_uses_emergency_compaction_when_aggressive_cannot_help() {
    let config = AgentLoopConfig {
        max_context_tokens: Some(20_000),
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = OverflowThenSuccessProvider {
        failures_before_success: 1,
        call_count: AtomicUsize::new(0),
        seen_max_tokens: Mutex::new(Vec::new()),
    };
    let large = "history ".repeat(1_200);
    let messages = vec![
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user("Please continue"),
    ];
    let (tx, mut rx) = mpsc::channel(16);

    let result = agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .unwrap();

    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            warnings.push(msg);
        }
    }

    assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
    assert!(result.llm_error.is_none());
    assert!(result.total_text.contains("Recovered after overflow."));
    assert!(warnings
        .iter()
        .any(|msg| msg.contains("emergency compaction")));
}

#[tokio::test]
async fn test_prompt_overflow_retry_reduces_response_budget() {
    let config = AgentLoopConfig {
        max_context_tokens: Some(20_000),
        max_tokens: 16_384,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = OverflowThenSuccessProvider {
        failures_before_success: 1,
        call_count: AtomicUsize::new(0),
        seen_max_tokens: Mutex::new(Vec::new()),
    };
    let large = "history ".repeat(1_200);
    let messages = vec![
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user(large.clone()),
        Message::assistant(large.clone()),
        Message::user("Please continue"),
    ];

    let result = agent
        .run(&provider, &executor, messages, vec![])
        .await
        .unwrap();
    let seen_max_tokens = provider
        .seen_max_tokens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    assert!(result.llm_error.is_none());
    assert_eq!(seen_max_tokens.len(), 2);
    assert!(seen_max_tokens[1] < seen_max_tokens[0]);
    assert_eq!(seen_max_tokens[1], 8_192);
}

#[tokio::test]
async fn test_agent_loop_handles_summary_compaction() {
    let config = AgentLoopConfig {
        max_context_tokens: Some(8_000),
        max_tokens: 1,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let executor = MockExecutor { results: vec![] };
    let provider = SummaryThenFinalProvider {
        call_count: AtomicUsize::new(0),
        request_kinds: Mutex::new(Vec::new()),
    };
    let mut messages = vec![Message::user("anchor")];
    for i in 0..80 {
        if i % 2 == 0 {
            messages.push(Message::assistant("A".repeat(10_000)));
        } else {
            messages.push(Message::user("B".repeat(10_000)));
        }
    }
    messages.push(Message::assistant("recent assistant tail"));
    messages.push(Message::user("continue"));

    let result = agent
        .run(&provider, &executor, messages, vec![])
        .await
        .unwrap();

    assert_eq!(provider.call_count.load(Ordering::SeqCst), 2);
    let kinds = provider
        .request_kinds
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        kinds.first().copied().flatten(),
        Some(ModelRequestKind::Auxiliary)
    );
    assert!(result.total_text.contains("Done after summary compaction."));
    assert!(result
        .messages
        .iter()
        .any(|message| message.text_content().contains("earlier turns explored")));
}
