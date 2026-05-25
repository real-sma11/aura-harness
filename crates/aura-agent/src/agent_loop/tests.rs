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
    assert_eq!(config.auto_build_cooldown, 2);
    assert_eq!(config.thinking_taper_after, 2);
    assert!((config.thinking_taper_factor - 0.6).abs() < f64::EPSILON);
    // Floor raised from 1024 → 6144 to fit a full-size tool-call JSON
    // (harness observed `edit_file` truncations at ~2.5 KB / ~1000
    // tokens plus preceding reasoning). See `constants::THINKING_MIN_BUDGET`.
    assert_eq!(config.thinking_min_budget, 6144);
    // The default config carries no exploration-reset signal; only
    // `execute_task_tracked` wires one through.
    assert!(config.phase_reset_signal.is_none());
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
    assert_eq!(kinds.first().copied().flatten(), Some(ModelRequestKind::Auxiliary));
    assert!(result.total_text.contains("Done after summary compaction."));
    assert!(
        result
            .messages
            .iter()
            .any(|message| message.text_content().contains("earlier turns explored"))
    );
}

// ------------------------------------------------------------------
// phase_reset_signal + LoopState::begin_iteration tests
// ------------------------------------------------------------------

/// The plan's headline regression check: drive `blocking_ctx`
/// exploration up, flip the shared `Arc<AtomicBool>`, run one
/// `begin_iteration` tick, then assert (a) the exploration counter is
/// zero, and (b) `ReadGuardState` is empty. The implement-phase
/// allowance bonus was deleted along with `exploration_allowance`
/// (the hard cap was already neutralized to `usize::MAX`), so the
/// reset is now purely a counter clear.
#[test]
fn phase_reset_clears_exploration_budget() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::default()
    };
    let mut state = super::LoopState::new(&config, vec![]);

    state.blocking_ctx.exploration_count = 40;
    state.exploration_state.count = 40;
    state.read_guard.record_full_read("foo.rs");
    state.read_guard.record_full_read("foo.rs");
    state.read_guard.record_range_read("bar.rs");
    state.exploration_compaction_done = true;

    state
        .blocking_ctx
        .written_paths
        .insert("written.rs".into());

    signal.store(true, Ordering::Release);

    state.begin_iteration(&config, 5);

    assert_eq!(state.blocking_ctx.exploration_count, 0);
    assert_eq!(state.exploration_state.count, 0);

    assert_eq!(state.read_guard.full_read_count("foo.rs"), 0);
    assert_eq!(state.read_guard.range_read_count("bar.rs"), 0);

    assert!(!state.exploration_compaction_done);
    assert!(
        !signal.load(Ordering::Acquire),
        "signal must be consumed by begin_iteration"
    );
    assert!(
        state.blocking_ctx.written_paths.contains("written.rs"),
        "written_paths must be preserved across signal reset"
    );
}

/// Companion: when the signal is wired but not flipped, the reset
/// branch must not fire — the loop's normal counters keep ticking.
#[test]
fn begin_iteration_does_not_reset_when_signal_unset() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::default()
    };
    let mut state = super::LoopState::new(&config, vec![]);
    state.blocking_ctx.exploration_count = 10;
    state.exploration_state.count = 10;

    state.begin_iteration(&config, 5);

    assert_eq!(state.blocking_ctx.exploration_count, 10);
    assert_eq!(state.exploration_state.count, 10);
}

/// Companion: without a wired signal (the chat path), begin_iteration
/// must not touch exploration counters at all.
#[test]
fn begin_iteration_no_op_when_no_signal_configured() {
    let config = AgentLoopConfig {
        phase_reset_signal: None,
        ..AgentLoopConfig::default()
    };
    let mut state = super::LoopState::new(&config, vec![]);
    state.blocking_ctx.exploration_count = 40;

    state.begin_iteration(&config, 5);

    assert_eq!(state.blocking_ctx.exploration_count, 40);
}

/// Sanity: with the read-only / force-tool steering removed by the
/// cook-loop-fix strip (2026-05), `build_request` must always
/// produce `ToolChoice::Auto` regardless of any internal state.
#[test]
fn build_request_always_emits_tool_choice_auto() {
    use aura_reasoner::{Message, ToolChoice};

    let config = AgentLoopConfig {
        thinking_budget: Some(8_192),
        max_tokens: 16_384,
        ..AgentLoopConfig::default()
    };
    let state = super::LoopState::new(&config, vec![Message::user("hi")]);
    let request = state
        .build_request(&config, &[], 5)
        .expect("build_request must succeed");
    assert!(
        matches!(request.tool_choice, ToolChoice::Auto),
        "tool_choice must always be Auto after the cook-loop-fix strip; got {:?}",
        request.tool_choice,
    );
}

/// Regression guard for the submit_plan deadlock fix: the agent
/// loop's signal observer must flip `BlockingContext::plan_submitted`
/// in addition to resetting the counters, so downstream telemetry
/// keeps observing the implement-phase transition. The exploration
/// hard block that this latch used to gate has since been removed
/// along with `exploration_allowance` threading, but the latch
/// itself is preserved for handshake purposes.
#[test]
fn phase_reset_arms_plan_submitted_latch() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::default()
    };
    let mut state = super::LoopState::new(&config, vec![]);
    assert!(
        !state.blocking_ctx.plan_submitted,
        "latch must default to false so pre-plan exploration stays soft"
    );

    signal.store(true, Ordering::Release);
    state.begin_iteration(&config, 1);

    assert!(
        state.blocking_ctx.plan_submitted,
        "begin_iteration must arm the latch when it observes the reset signal"
    );
}


#[tokio::test]
async fn dev_loop_endturn_with_no_writes_terminates_immediately() {
    // The cook-loop-fix strip (2026-05) removed the dev-loop EndTurn
    // intercept escalation. A dev-loop task that ends its first turn
    // with `EndTurn` must now exit the loop on that turn — no nudges,
    // no force-tool-choice escalation, no thinking-disable on the
    // next iteration.
    let executor = MockExecutor { results: vec![] };

    let provider = MockProvider::new().with_response(MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("I'm thinking about the task.")],
        usage: Usage::new(100, 20),
    });

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        dev_loop_completion_required: true,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("implement bar")];
    let tools = vec![
        ToolDefinition::new(
            "read_file",
            "Read a file",
            serde_json::json!({"type": "object"}),
        ),
        ToolDefinition::new(
            "write_file",
            "Write a file",
            serde_json::json!({"type": "object"}),
        ),
    ];

    let (tx, mut rx) = mpsc::channel(64);
    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 1,
        "EndTurn must terminate the dev-loop on the first occurrence; no intercept",
    );
    assert!(result.file_changes.is_empty());

    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !(msg.contains("ended your turn without writing")
                    || msg.contains("Second EndTurn without progress")
                    || msg.contains("Third EndTurn without progress")),
                "no dev-loop intercept nudge may fire after the strip; got: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn dev_loop_endturn_after_write_terminates_cleanly() {
    // Once any file write lands, `dev_loop_completion_required`
    // must allow EndTurn to terminate the loop on its own — no
    // intercept, no escalation.
    //
    //   Iter 0: write_file (ToolUse, success)   -> had_any_file_write=true
    //   Iter 1: text only  (EndTurn)            -> exits cleanly
    let executor = MockExecutor {
        results: vec![ToolCallResult::success("call_write", "wrote 12 bytes")],
    };

    let provider = MockProvider::new()
        .with_response(MockResponse {
            stop_reason: StopReason::ToolUse,
            content: vec![ContentBlock::tool_use(
                "call_write",
                "write_file",
                serde_json::json!({"path": "src/new.rs", "content": "pub fn bar() {}"}),
            )],
            usage: Usage::new(100, 20),
        })
        .with_response(MockResponse {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text("Done.")],
            usage: Usage::new(150, 10),
        });

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        dev_loop_completion_required: true,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("implement bar")];
    let tools = vec![ToolDefinition::new(
        "write_file",
        "Write a file",
        serde_json::json!({"type": "object"}),
    )];

    let (tx, mut rx) = mpsc::channel(64);
    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 2,
        "loop must exit on the EndTurn that follows the write; no intercept"
    );
    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !(msg.contains("ended your turn without writing")
                    || msg.contains("Second EndTurn without progress")
                    || msg.contains("Third EndTurn without progress")),
                "no dev-loop intercept nudge may fire once a write has happened; got: {msg}"
            );
        }
    }
}

/// End-to-end Phase B + Phase E integration: an `apply_patch` call
/// against the real `TaskToolExecutor::handle_apply_patch` handler
/// must flip `had_any_file_write = true` so the very next EndTurn
/// terminates the loop cleanly with zero intercept escalations.
///
/// This is the regression pin that the unified `apply_patch` write
/// primitive correctly feeds the existing `FileOp` pipeline:
///
///   Iter 0: apply_patch (ToolUse) — parser → executor → real disk write,
///           handler emits one FileChange per Add/Update/Delete,
///           `tool_pipeline::track_tool_effects` sets had_any_file_write
///   Iter 1: text only  (EndTurn) — exits cleanly, no nudge fires
///
/// If a future refactor breaks the FileOp emission inside
/// `handle_apply_patch` (e.g. forgets to set `file_changes` on the
/// `ToolCallResult`), this test catches it: the EndTurn would be
/// intercepted, the loop would run more than 2 iterations, and the
/// warning channel would carry the dev-loop nudge text.
#[tokio::test]
async fn dev_loop_apply_patch_success_terminates_cleanly() {
    use crate::task_executor::TaskToolExecutor;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    // Real workspace on disk so the executor's filesystem path
    // resolution and atomic write phase actually exercise.
    let tmp = tempfile::TempDir::new().expect("create tempdir");
    let workspace = tmp.path().to_path_buf();

    // No-op inner executor — the apply_patch tool never reaches it
    // because `TaskToolExecutor::execute` intercepts the call in its
    // pre-dispatch arm and dispatches to `handle_apply_patch`. The
    // mock-runner is wired through `task_executor::tests` style
    // construction.
    struct NoOpInner;
    #[async_trait::async_trait]
    impl AgentToolExecutor for NoOpInner {
        async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
            tool_calls
                .iter()
                .map(|tc| ToolCallResult::success(&tc.id, "ok"))
                .collect()
        }
    }

    #[derive(Debug, Default)]
    struct NoTestRunner;
    #[async_trait::async_trait]
    impl crate::task_executor::TaskTestRunner for NoTestRunner {
        async fn run_tests(
            &self,
            _project_root: &std::path::Path,
            _command: &str,
        ) -> anyhow::Result<crate::verify::TestSuiteOutcome> {
            Ok(crate::verify::TestSuiteOutcome::default())
        }
    }

    let executor = TaskToolExecutor {
        inner: Arc::new(NoOpInner),
        project_folder: workspace.to_string_lossy().into_owned(),
        build_command: None,
        test_command: None,
        test_command_override: None,
        task_context: String::new(),
        tracked_file_ops: Default::default(),
        notes: Default::default(),
        follow_ups: Default::default(),
        stub_fix_attempts: Default::default(),
        test_gate_attempts: Default::default(),
        test_runner: Arc::new(NoTestRunner),
        disable_test_gate: true,
        task_phase: Arc::new(tokio::sync::Mutex::new(
            crate::planning::TaskPhase::Implementing {
                plan: crate::planning::TaskPlan::empty(),
            },
        )),
        self_review: Default::default(),
        event_tx: None,
        no_changes_needed: Default::default(),
        dod_test_gate_exhausted: Default::default(),
        recent_tool_outcomes: Default::default(),
        reset_explore_on_phase_change: Arc::new(AtomicBool::new(false)),
    };

    // Build a small codex-envelope patch that adds two files.
    // Newline-explicit to dodge the CRLF / line-continuation pitfalls
    // that the parser tests already pin.
    let patch = "*** Begin Patch\n\
                 *** Add File: src/alpha.rs\n\
                 +pub fn alpha() -> u32 { 1 }\n\
                 *** Add File: src/beta.rs\n\
                 +pub fn beta() -> u32 { 2 }\n\
                 *** End Patch\n";

    let provider = MockProvider::new()
        .with_response(MockResponse {
            stop_reason: StopReason::ToolUse,
            content: vec![ContentBlock::tool_use(
                "call_apply_patch",
                "apply_patch",
                serde_json::json!({ "patch": patch }),
            )],
            usage: Usage::new(120, 30),
        })
        .with_response(MockResponse {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text("Applied alpha + beta.")],
            usage: Usage::new(150, 20),
        });

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        dev_loop_completion_required: true,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("add alpha and beta")];
    let tools = vec![ToolDefinition::new(
        "apply_patch",
        "Unified patch tool",
        serde_json::json!({"type": "object"}),
    )];

    let (tx, mut rx) = mpsc::channel(64);
    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 2,
        "apply_patch success must flip had_any_file_write; EndTurn on \
         iter 2 must terminate the loop without an intercept escalation"
    );

    let mut nudge_count = 0_usize;
    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            if msg.contains("ended your turn without writing")
                || msg.contains("Second EndTurn without progress")
                || msg.contains("Third EndTurn without progress")
            {
                nudge_count += 1;
            }
            warnings.push(msg);
        }
    }
    assert_eq!(
        nudge_count, 0,
        "endturn_intercept_count must remain 0 after a successful \
         apply_patch; observed warnings: {warnings:?}"
    );

    // Filesystem-level confirmation: the executor really wrote the
    // files. If `handle_apply_patch` ever silently returned without
    // dispatching to `aura_tools::apply_patch::execute_apply_patch`,
    // this catches it.
    assert!(workspace.join("src/alpha.rs").exists(), "src/alpha.rs must exist on disk");
    assert!(workspace.join("src/beta.rs").exists(), "src/beta.rs must exist on disk");
}

#[tokio::test]
async fn chat_mode_endturn_terminates_immediately() {
    // Regression guard for the Phase B chat-mode invariant: a normal
    // chat session ("read one file, answer the question") must still
    // exit cleanly on the first EndTurn. `dev_loop_completion_required`
    // defaults to false; the intercept must not fire.
    //
    //   Iter 0: read_file (ToolUse)   -> consecutive_read_only counter -> 1
    //   Iter 1: text only (EndTurn)   -> exits IMMEDIATELY (chat mode)
    let executor = MockExecutor {
        results: vec![ToolCallResult::success("call_read", "fn foo() {}")],
    };

    let provider = MockProvider::new()
        .with_response(MockResponse {
            stop_reason: StopReason::ToolUse,
            content: vec![ContentBlock::tool_use(
                "call_read",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            )],
            usage: Usage::new(100, 20),
        })
        .with_response(MockResponse {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text("It's a one-line stub.")],
            usage: Usage::new(150, 30),
        });

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        // dev_loop_completion_required defaults to false — explicit
        // here for documentation.
        ..AgentLoopConfig::default()
    };
    assert!(!config.dev_loop_completion_required);
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("what does src/lib.rs contain?")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let (tx, mut rx) = mpsc::channel(64);
    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 2,
        "chat mode must exit on the EndTurn that follows the read"
    );
    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !(msg.contains("ended your turn without writing")
                    || msg.contains("Second EndTurn without progress")
                    || msg.contains("Third EndTurn without progress")),
                "chat mode must not emit dev-loop intercept nudges; got: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn dev_loop_endturn_after_task_done_terminates_cleanly() {
    // A successful `task_done` (DoD gates passed) flips
    // `task_done_completed = true` via the `stop_loop` handshake on
    // the resulting tool call. Even with no write in this run, the
    // dev-loop intercept must NOT fire — `task_done` is the
    // explicit "no-changes-needed" escape.
    //
    //   Iter 0: task_done (ToolUse, stop_loop=true) -> exits via tool stop
    let task_done_result = ToolCallResult {
        tool_use_id: "call_done".to_string(),
        content: r#"{"status":"completed"}"#.to_string(),
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: true,
        file_changes: Vec::new(),
    };
    let executor = MockExecutor {
        results: vec![task_done_result],
    };

    let provider = MockProvider::new().with_response(MockResponse {
        stop_reason: StopReason::ToolUse,
        content: vec![ContentBlock::tool_use(
            "call_done",
            "task_done",
            serde_json::json!({"no_changes_needed": true, "notes": "nothing to change"}),
        )],
        usage: Usage::new(100, 20),
    });

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        dev_loop_completion_required: true,
        ..AgentLoopConfig::default()
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("verify the bar is already implemented")];
    let tools = vec![ToolDefinition::new(
        "task_done",
        "Signal task completion",
        serde_json::json!({"type": "object"}),
    )];

    let (tx, mut rx) = mpsc::channel(64);
    let result = agent
        .run_with_events(&provider, &executor, messages, tools, Some(tx), None)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 1,
        "task_done with stop_loop=true must exit on its own iteration; no intercept"
    );
    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !(msg.contains("ended your turn without writing")
                    || msg.contains("Second EndTurn without progress")
                    || msg.contains("Third EndTurn without progress")),
                "successful task_done must not emit a dev-loop intercept nudge; got: {msg}"
            );
        }
    }
}

