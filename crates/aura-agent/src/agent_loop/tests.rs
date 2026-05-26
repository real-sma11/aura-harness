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
    let config = AgentLoopConfig::for_agent("claude-test-model");
    // Default flows from `aura_core::MAX_TURNS` — the single source of
    // truth shared with the runtime session, agent runner, subagent
    // budgets, and integration-test harness. Termination is still
    // driven by `EndTurn`, the credit budget, or cooperative
    // cancellation; this assertion just pins the per-turn cap and the
    // companion E.1 caps to the same canonical value.
    assert_eq!(config.max_iterations, aura_core::MAX_TURNS as usize);
    assert_eq!(config.max_turns_per_task, aura_core::MAX_TURNS);
    assert_eq!(config.max_iterations_per_task, aura_core::MAX_TURNS);
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
    let config = AgentLoopConfig::for_agent("claude-test-model");
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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

    let config = AgentLoopConfig::for_agent("claude-test-model");
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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
    let config = AgentLoopConfig::for_agent("claude-test-model");
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
        // PromptTooLong retry + emergency compaction live on the
        // legacy buffered sampling path. The pump path port is a
        // follow-up; pin this test to the legacy path until then.
        use_stream_pump: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        use_stream_pump: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        use_stream_pump: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        use_stream_pump: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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

// ------------------------------------------------------------------
// phase_reset_signal + LoopState::begin_iteration tests
// ------------------------------------------------------------------

/// The reset signal handshake: drive the exploration counter up,
/// flip the shared `Arc<AtomicBool>`, run one `begin_iteration`
/// tick, then assert the exploration counter is cleared. The
/// `ReadGuardState` / `BlockingContext` resets that used to be
/// exercised here were removed along with the dead detector
/// modules (cook-loop-fix follow-up, 2026-05) — the reset is now
/// purely an exploration-counter clear plus
/// `exploration_compaction_done` rearm.
#[test]
fn phase_reset_clears_exploration_budget() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let mut state = super::LoopState::new(&config, vec![]);

    state.exploration_state.count = 40;
    state.exploration_compaction_done = true;

    signal.store(true, Ordering::Release);

    state.begin_iteration(&config, 5);

    assert_eq!(state.exploration_state.count, 0);
    assert!(!state.exploration_compaction_done);
    assert!(
        !signal.load(Ordering::Acquire),
        "signal must be consumed by begin_iteration"
    );
}

/// Phase 2: the `phase_reset_signal` flip on iteration > 0 must
/// latch `submit_plan_called` so the effort policy can drop to
/// `Low` once a plan has actually been accepted. The iteration-0
/// flip is the task-start pre-seed (see
/// `agent_runner::execute_task_tracked`) and must NOT latch the
/// signal — confirms the heuristic that separates "task start" from
/// "real submit_plan accept".
#[test]
fn submit_plan_signal_latches_only_on_iteration_after_zero() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let mut state = super::LoopState::new(&config, vec![]);

    // Task-start pre-seed: iteration 0 observes the flip but the
    // latch stays off (this is the runner zeroing exploration, not
    // a real submit_plan accept).
    signal.store(true, Ordering::Release);
    state.begin_iteration(&config, 0);
    assert!(
        !state.submit_plan_called,
        "iteration-0 reset is the task-start pre-seed; must not latch"
    );

    // Real submit_plan accept (executor flips the signal mid-run):
    // iteration > 0 observes it and the latch turns on.
    signal.store(true, Ordering::Release);
    state.begin_iteration(&config, 4);
    assert!(
        state.submit_plan_called,
        "iteration > 0 reset is a real submit_plan accept; must latch"
    );

    // Latch is cumulative — subsequent iterations keep it set even
    // when the signal is not flipped again.
    state.begin_iteration(&config, 5);
    assert!(state.submit_plan_called, "latch is cumulative");
}

/// Companion: when the signal is wired but not flipped, the reset
/// branch must not fire — the exploration counter keeps ticking.
#[test]
fn begin_iteration_does_not_reset_when_signal_unset() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let signal = Arc::new(AtomicBool::new(false));
    let config = AgentLoopConfig {
        phase_reset_signal: Some(Arc::clone(&signal)),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let mut state = super::LoopState::new(&config, vec![]);
    state.exploration_state.count = 10;

    state.begin_iteration(&config, 5);

    assert_eq!(state.exploration_state.count, 10);
}

/// Companion: without a wired signal (the chat path), begin_iteration
/// must not touch exploration counters at all.
#[test]
fn begin_iteration_no_op_when_no_signal_configured() {
    let config = AgentLoopConfig {
        phase_reset_signal: None,
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let mut state = super::LoopState::new(&config, vec![]);
    state.exploration_state.count = 40;

    state.begin_iteration(&config, 5);

    assert_eq!(state.exploration_state.count, 40);
}

// ------------------------------------------------------------------
// Phase 3 — parallel tool calls per assistant turn
// ------------------------------------------------------------------

/// Three `read_file` `tool_use` blocks in a single assistant turn
/// must all execute in one loop iteration. Confirms aura's existing
/// `extract_tool_calls` already iterates `Vec<ContentBlock::ToolUse>`
/// — Phase 3 only had to enable the wire flag; the pipeline side
/// was already correct. The `track_tool_effects` integration from
/// Phase 1.A was wired into the same multi-tool pipeline, so this
/// also pins that 3 tool_results land back in the message history
/// without splitting across iterations.
#[tokio::test]
async fn three_read_file_calls_execute_in_one_iteration() {
    use aura_reasoner::{ContentBlock, ToolResultContent};

    let executor = MockExecutor {
        results: vec![
            ToolCallResult {
                tool_use_id: "placeholder".to_string(),
                content: "ok-1".to_string(),
                is_error: false,
                kind: aura_core::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
            },
            ToolCallResult {
                tool_use_id: "placeholder".to_string(),
                content: "ok-2".to_string(),
                is_error: false,
                kind: aura_core::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
            },
            ToolCallResult {
                tool_use_id: "placeholder".to_string(),
                content: "ok-3".to_string(),
                is_error: false,
                kind: aura_core::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
            },
        ],
    };

    // Single assistant turn with three parallel tool_use blocks.
    let parallel_response = MockResponse {
        stop_reason: StopReason::ToolUse,
        content: vec![
            ContentBlock::tool_use("toolu_1", "read_file", serde_json::json!({"path": "a.rs"})),
            ContentBlock::tool_use("toolu_2", "read_file", serde_json::json!({"path": "b.rs"})),
            ContentBlock::tool_use("toolu_3", "read_file", serde_json::json!({"path": "c.rs"})),
        ],
        usage: Usage::new(100, 30),
    };

    let provider = MockProvider::new()
        .with_response(parallel_response)
        .with_response(MockResponse::text("Done after one parallel batch."));

    let config = AgentLoopConfig {
        system_prompt: "You are a test agent".to_string(),
        // The buffered path calls `execute()` once per batch of
        // tool_use blocks, so the MockExecutor's positional zip
        // returns [ok-1, ok-2, ok-3]. The pump path spawns each
        // tool individually (matching codex's stream-level overlap)
        // which would re-trigger the executor once per call and
        // collapse the positional zip to [ok-1, ok-1, ok-1] —
        // covered by the pump-side parity_tests instead.
        use_stream_pump: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("Read three files.")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    // Two iterations total: one tool-use turn (carrying all three
    // calls) + one final EndTurn. NOT four — the regression the
    // Phase 3 wire flag prevents is the agent emitting one tool per
    // iteration and ballooning the iteration count.
    assert_eq!(
        result.iterations, 2,
        "three parallel tool_use blocks must execute in a single iteration; got {} iterations",
        result.iterations
    );

    // All three tool results must land in the message history.
    let tool_result_count: usize = result
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_result_count, 3,
        "all three tool_use blocks must produce tool_result messages in the same iteration"
    );

    // And their contents must match the per-call executor outputs
    // (no batching collisions / clobbered results).
    let result_contents: std::collections::HashSet<String> = result
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| {
            if let ContentBlock::ToolResult { content, .. } = b {
                if let ToolResultContent::Text(s) = content {
                    return Some(s.clone());
                }
            }
            None
        })
        .collect();
    assert!(result_contents.contains("ok-1"), "result ok-1 missing");
    assert!(result_contents.contains("ok-2"), "result ok-2 missing");
    assert!(result_contents.contains("ok-3"), "result ok-3 missing");
}

// ------------------------------------------------------------------
// Phase 2 — `compute_thinking_effort` dev-loop policy
// ------------------------------------------------------------------

/// `disable_thinking_iteration_0: true` + iteration 0 → `Off`,
/// regardless of any other state. Preserves the runner's
/// fast-first-tool-call behaviour while routing through the new
/// codex-style effort knob instead of the legacy max_tokens clamp.
#[test]
fn effort_off_when_disable_thinking_iteration_0() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig {
        disable_thinking_iteration_0: true,
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let state = super::LoopState::new(&config, vec![]);
    assert_eq!(
        state.compute_thinking_effort(&config, 0),
        ThinkingEffort::Off
    );
}

/// Default analysis turn (iteration 0 without
/// `disable_thinking_iteration_0`) gets `Medium` so the agent has
/// budget to plan before its first tool call. Mirrors codex's
/// default `reasoning.effort = medium`.
#[test]
fn effort_medium_on_iteration_0_no_disable() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig {
        disable_thinking_iteration_0: false,
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let state = super::LoopState::new(&config, vec![]);
    assert_eq!(
        state.compute_thinking_effort(&config, 0),
        ThinkingEffort::Medium
    );
}

/// Once the first write_file/edit_file/delete_file has landed, drop
/// to `Low`. Caps the thinking spiral that amplifies follow-up
/// iterations after forward motion has already happened.
#[test]
fn effort_low_after_first_write() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig::for_agent("claude-test-model");
    let mut state = super::LoopState::new(&config, vec![]);
    state.had_any_file_write = true;
    assert_eq!(
        state.compute_thinking_effort(&config, 3),
        ThinkingEffort::Low
    );
}

/// Once `submit_plan` has been accepted (signal observed on
/// iteration > 0), drop to `Low`. Codex's analogous behaviour is to
/// keep effort low during the implementation phase.
#[test]
fn effort_low_after_submit_plan() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig::for_agent("claude-test-model");
    let mut state = super::LoopState::new(&config, vec![]);
    state.submit_plan_called = true;
    assert_eq!(
        state.compute_thinking_effort(&config, 2),
        ThinkingEffort::Low
    );
}

/// Non-iteration-0 iterations without writes, plans, or continuation
/// pressure default to `Medium`. Confirms the policy doesn't
/// silently fall to `Low` without one of the explicit triggers.
#[test]
fn effort_medium_default_after_iteration_zero() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig::for_agent("claude-test-model");
    let state = super::LoopState::new(&config, vec![]);
    assert_eq!(
        state.compute_thinking_effort(&config, 3),
        ThinkingEffort::Medium
    );
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
        ..AgentLoopConfig::for_agent("claude-test-model")
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

#[tokio::test]
async fn chat_mode_endturn_terminates_immediately() {
    // Regression guard: a normal chat session ("read one file,
    // answer the question") exits cleanly on the first EndTurn.
    // Codex parity: the harness no longer intercepts empty
    // terminations, so this is just a straightforward EndTurn
    // termination test.
    //
    //   Iter 0: read_file (ToolUse)
    //   Iter 1: text only (EndTurn) -> exits IMMEDIATELY
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
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
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

// ------------------------------------------------------------------
// Codex parity: trust EndTurn (replacement tests)
// ------------------------------------------------------------------

/// Codex parity #1: a single `EndTurn` with no tool calls and no
/// writes must terminate the loop cleanly on the first iteration.
/// Pre-codex-parity the harness would intercept the empty EndTurn
/// and force-inject a continuation prompt up to ~24 times before
/// failing with `task_blocked`; the new behaviour trusts the model.
#[tokio::test]
async fn agentloop_endturn_terminates_without_writes() {
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::new().with_response(MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("I'm done; nothing to write.")],
        usage: Usage::new(50, 10),
    });

    let agent = AgentLoop::new(AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    });
    let messages = vec![Message::user("verify the bar is already implemented")];
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
        result.iterations, 1,
        "EndTurn must terminate the loop on the first sampling regardless of writes"
    );
    assert!(!result.stalled, "no stall flag without an explicit budget overrun");
    assert!(
        result.llm_error.is_none(),
        "no llm_error envelope — the model owns the exit signal"
    );

    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !msg.contains("<harness_continuation"),
                "no <harness_continuation> envelope must surface; got: {msg}"
            );
        }
    }
}

/// Codex parity #2: a write followed by `EndTurn` terminates on the
/// EndTurn turn (iteration 2). Confirms the loop continues after the
/// tool batch and exits cleanly on the next stop signal without
/// continuation injection.
#[tokio::test]
async fn agentloop_endturn_terminates_after_partial_write() {
    let executor = MockExecutor {
        results: vec![ToolCallResult {
            tool_use_id: "call_write".to_string(),
            content: "wrote new.rs".to_string(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: vec![crate::types::FileChange {
                path: "src/new.rs".to_string(),
                kind: crate::types::FileChangeKind::Create,
                lines_added: 1,
                lines_removed: 0,
            }],
        }],
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
            content: vec![ContentBlock::text("Done after write.")],
            usage: Usage::new(150, 30),
        });

    let agent = AgentLoop::new(AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    });
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
        "first iter writes, second iter EndTurn terminates"
    );
    assert!(
        !result.file_changes.is_empty(),
        "the write must be recorded on the result"
    );
    assert!(result.llm_error.is_none());

    while let Ok(event) = rx.try_recv() {
        if let AgentLoopEvent::Warning(msg) = event {
            assert!(
                !msg.contains("<harness_continuation"),
                "no continuation envelopes after a clean EndTurn; got: {msg}"
            );
        }
    }
}

/// Codex parity #3: an `EndTurn` arriving without any `task_done`
/// call and without any writes still terminates cleanly. Pre-codex-
/// parity the absence of `task_done` would gate the EndTurn intercept
/// open and the loop would keep nudging.
#[tokio::test]
async fn agentloop_endturn_terminates_when_no_task_done_called() {
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::new().with_response(MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("Looks fine; no edits needed.")],
        usage: Usage::new(30, 8),
    });

    let agent = AgentLoop::new(AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    });
    let messages = vec![Message::user("check if foo handles None correctly")];
    let tools = vec![ToolDefinition::new(
        "task_done",
        "Signal task completion",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(
        result.llm_error.is_none(),
        "no task_blocked llm_error — the harness no longer escalates on missing task_done"
    );
    assert!(
        !result.stalled,
        "stalled must stay false when the model itself chose to terminate"
    );
}

// ------------------------------------------------------------------
// Layer E.1 — nested loop topology (task -> turn -> sampling)
// ------------------------------------------------------------------

/// The turn loop must consume every `ToolUse` sampling without
/// breaking, then break cleanly on the trailing `EndTurn`. Codex-shape
/// regression: pinned because the polarity flip from
/// `for iteration { … if break { break; } }` to
/// `loop { … if !needs_follow_up { break; } }` would have been a
/// silent behavior change if `dispatch_stop_reason`'s "loop should
/// break" semantics had been mis-translated into `needs_follow_up`.
#[tokio::test]
async fn turn_continues_while_needs_follow_up_true() {
    let executor = MockExecutor {
        results: vec![
            ToolCallResult::success("ph1", "alpha"),
            ToolCallResult::success("ph2", "beta"),
            ToolCallResult::success("ph3", "gamma"),
        ],
    };

    let provider = MockProvider::new()
        .with_response(MockResponse::tool_use(
            "tc_1",
            "read_file",
            serde_json::json!({"path": "a.rs"}),
        ))
        .with_response(MockResponse::tool_use(
            "tc_2",
            "read_file",
            serde_json::json!({"path": "b.rs"}),
        ))
        .with_response(MockResponse::tool_use(
            "tc_3",
            "read_file",
            serde_json::json!({"path": "c.rs"}),
        ))
        .with_response(MockResponse::text("Done after three reads."));

    let config = AgentLoopConfig {
        system_prompt: "test agent".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("read three files")];
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
        result.iterations, 4,
        "three ToolUse follow-ups plus one terminating EndTurn must \
         all run inside one turn — total sampling requests = 4"
    );
    assert!(
        result.total_text.contains("Done after three reads."),
        "the final EndTurn message must surface on the result"
    );
    assert!(
        result.llm_error.is_none(),
        "no llm_error expected on the happy path"
    );
}

/// Counterpart to `turn_continues_while_needs_follow_up_true`: a
/// single `EndTurn` (no tool calls) must break the turn loop on the
/// first sampling. Pinned so the polarity flip cannot accidentally
/// re-introduce an off-by-one extra sampling on the "model already
/// said stop" path.
#[tokio::test]
async fn turn_breaks_when_model_says_stop_and_no_continuation() {
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::simple_response("Nothing to do.");

    let config = AgentLoopConfig {
        system_prompt: "test agent".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("are you there?")];
    let tools = vec![];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 1,
        "a clean EndTurn with no follow-up must break the turn loop \
         on the first sampling"
    );
    assert!(result.total_text.contains("Nothing to do."));
    assert!(!result.stalled);
    assert!(result.llm_error.is_none());
}

/// Per-task hard ceiling regression: a misconfigured
/// `max_turns_per_task = 0` must surface as a typed
/// `AgentError::TurnBudgetExceeded` rather than silently returning
/// an empty result. Pinned because the cap trips inside
/// `task::run_task` before the first `run_turn` call and the Err
/// propagates up through `AgentLoop::run` — callers that rely on
/// the pre-E.1 "always-Ok" contract for normal LLM/tool errors must
/// still see a structured failure for budget overruns.
#[tokio::test]
async fn turn_budget_exceeded_surfaces_typed_error_when_max_turns_zero() {
    let executor = MockExecutor { results: vec![] };
    let provider = MockProvider::simple_response("never reached");

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        max_turns_per_task: 0,
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("anything")];
    let tools = vec![];

    let err = agent
        .run(&provider, &executor, messages, tools)
        .await
        .expect_err("max_turns_per_task=0 must trip TurnBudgetExceeded");

    match err {
        crate::AgentError::TurnBudgetExceeded { turn_index, .. } => {
            assert_eq!(turn_index, 0, "zero-budget cap trips at turn 0");
        }
        other => panic!("expected TurnBudgetExceeded, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// Layer E.2 — input queue (mid-task user steering)
// ------------------------------------------------------------------

/// User input arrives mid-turn. Expectation:
///
///   - Sampling 1 runs the original `user → "kick off"` message,
///     model emits `EndTurn`. From inside `provider.complete()` we
///     push a [`UserInput::Message`] onto the queue so that — at the
///     bottom of the turn loop — `has_pending() == true` and the
///     turn loop re-enters instead of breaking out cleanly.
///   - The next iteration drains the queued message into
///     `state.messages` (via [`crate::helpers::append_warning`], which
///     merges into the trailing user block to preserve Anthropic
///     `tool_use` / `tool_result` adjacency), then runs sampling 2.
///   - Sampling 2 emits a second `EndTurn` referencing the queued
///     content. After that, `has_pending() == false` and the turn
///     loop terminates cleanly.
///
/// Asserts the total iteration count, the queued message body
/// landing in the final message history, and that the provider only
/// got two calls (the loop did NOT spin extra rounds beyond the one
/// the queue triggered).
///
/// Uses a scripted-fakes provider (no `tokio::time::sleep` or wall
/// clock ordering) per Rule 7.3 — every event ordering assertion
/// here is provider-driven, not time-driven.
#[tokio::test]
async fn pending_input_extends_turn_loop() {
    use crate::session::input_queue::InputQueue;
    use crate::session::SessionId;
    use crate::{AgentRunnerHandle, UserInput};
    use aura_reasoner::{ModelRequest, ModelResponse, ProviderTrace, Usage};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct InjectMessageOnFirstCall {
        queue: Arc<InputQueue>,
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelProvider for InjectMessageOnFirstCall {
        fn name(&self) -> &'static str {
            "inject-message-on-first-call"
        }

        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            let text = if n == 0 {
                self.queue
                    .push(UserInput::Message(
                        "follow-up: please summarise what you just did".into(),
                    ))
                    .await
                    .expect("queue is open");
                "First call done."
            } else {
                "Second call after queued input."
            };
            Ok(ModelResponse::new(
                StopReason::EndTurn,
                Message::assistant(text),
                Usage::new(80, 20),
                ProviderTrace::new("e2-mock", 0),
            ))
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    let cancel = CancellationToken::new();
    let handle = AgentRunnerHandle::new(SessionId::new_v4(), cancel.clone());
    let provider = InjectMessageOnFirstCall {
        queue: handle.queue(),
        call_count: AtomicUsize::new(0),
    };
    let executor = MockExecutor { results: vec![] };

    let agent = AgentLoop::new(AgentLoopConfig {
        system_prompt: "test agent".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    });

    let result = agent
        .run_with_session(
            &provider,
            &executor,
            vec![Message::user("kick off")],
            vec![],
            None,
            Some(cancel.clone()),
            Some(&handle),
        )
        .await
        .expect("E.2 happy path must succeed");

    assert_eq!(
        provider.call_count.load(Ordering::SeqCst),
        2,
        "queued input must trigger exactly one extra sampling request"
    );
    assert_eq!(
        result.iterations, 2,
        "iteration counter monotonic across the queue-extended sampling"
    );
    assert!(
        result
            .total_text
            .contains("Second call after queued input."),
        "the second EndTurn message must surface on the result"
    );
    assert!(
        result.messages.iter().any(|msg| msg
            .text_content()
            .contains("follow-up: please summarise what you just did")),
        "queued user-input body must have been appended to the message history before sampling 2"
    );
    assert!(
        !cancel.is_cancelled(),
        "non-cancel user input must NOT fire the cancellation token"
    );
    assert!(
        !handle.has_pending(),
        "queue must be empty after the loop drains the message"
    );
}

/// [`UserInput::Cancel`] unwinds the active turn via the shared
/// cancellation token, without leaving the message history in a
/// half-written state.
///
///   - Sampling 1 runs the original user prompt, model emits a
///     `ToolUse` (so `needs_follow_up = true`). From inside
///     `provider.complete()` we push [`UserInput::Cancel`] onto the
///     queue, which fires the cancellation token before the inner
///     loop's post-sampling `is_cancelled` check.
///   - The post-call `is_cancelled` check inside
///     [`crate::agent_loop::sampling::run_sampling_request`] short-
///     circuits the tool dispatch and returns
///     `broke_for_error = true`. The turn loop unwinds without
///     entering another sampling.
///
/// Asserts the cancellation token is observed (cancelled), the tool
/// dispatch never fires (the executor would have panicked otherwise),
/// and the provider is not called a second time. Uses a scripted-
/// fakes provider so the ordering is fully deterministic (Rule 7.3).
#[tokio::test]
async fn cancel_unwinds_active_turn() {
    use crate::session::input_queue::InputQueue;
    use crate::session::SessionId;
    use crate::{AgentRunnerHandle, UserInput};
    use aura_reasoner::{ContentBlock, ModelRequest, ModelResponse, ProviderTrace, Usage};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct PanicOnExecute;
    #[async_trait::async_trait]
    impl AgentToolExecutor for PanicOnExecute {
        async fn execute(&self, _tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
            panic!("tool executor must NOT run after a Cancel push");
        }
    }

    struct CancelOnFirstCall {
        queue: Arc<InputQueue>,
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelProvider for CancelOnFirstCall {
        fn name(&self) -> &'static str {
            "cancel-on-first-call"
        }

        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                n, 0,
                "provider must NOT be called a second time after cancel"
            );
            // ToolUse (without follow-through) would normally make the
            // turn loop dispatch tools. We fire UserInput::Cancel
            // *before* returning so the post-call `is_cancelled` check
            // in `run_sampling_request` short-circuits the dispatch
            // (and the PanicOnExecute executor never gets a chance to
            // run). This pins the atomic-cancel invariant from Rule
            // 6.3: the partial response is never appended together
            // with a half-executed tool batch.
            self.queue
                .push(UserInput::Cancel)
                .await
                .expect("queue is open");
            Ok(ModelResponse::new(
                StopReason::ToolUse,
                Message {
                    role: aura_reasoner::Role::Assistant,
                    content: vec![ContentBlock::tool_use(
                        "toolu_cancelled",
                        "read_file",
                        serde_json::json!({"path": "ignored.rs"}),
                    )],
                },
                Usage::new(50, 10),
                ProviderTrace::new("e2-mock", 0),
            ))
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    let cancel = CancellationToken::new();
    let handle = AgentRunnerHandle::new(SessionId::new_v4(), cancel.clone());
    let provider = CancelOnFirstCall {
        queue: handle.queue(),
        call_count: AtomicUsize::new(0),
    };
    let executor = PanicOnExecute;

    let agent = AgentLoop::new(AgentLoopConfig {
        system_prompt: "test agent".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    });

    let result = agent
        .run_with_session(
            &provider,
            &executor,
            vec![Message::user("start a long task")],
            vec![ToolDefinition::new(
                "read_file",
                "Read a file",
                serde_json::json!({"type": "object"}),
            )],
            None,
            Some(cancel.clone()),
            Some(&handle),
        )
        .await
        .expect("cancel must unwind cleanly, not propagate as Err");

    assert!(
        cancel.is_cancelled(),
        "UserInput::Cancel must fire the shared cancellation token"
    );
    assert_eq!(
        provider.call_count.load(Ordering::SeqCst),
        1,
        "no second sampling request after cancel"
    );
    assert!(
        result.iterations <= 1,
        "loop must not pay for sampling beyond the cancel boundary"
    );
}

/// Closed-queue path: once [`AgentRunnerHandle::close`] runs, any
/// subsequent `send_user_input` surfaces a typed
/// [`crate::AgentError::InputQueueClosed`] with the originating
/// session id. Mirrors Rules 4.1 / 4.3: no silent drops on session
/// teardown.
#[tokio::test]
async fn send_user_input_after_close_returns_input_queue_closed() {
    use crate::session::SessionId;
    use crate::{AgentRunnerHandle, UserInput};
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();
    let session_id = SessionId::new_v4();
    let handle = AgentRunnerHandle::new(session_id, cancel);
    handle.close();
    let err = handle
        .send_user_input(UserInput::Message("late arrival".into()))
        .await
        .expect_err("send must fail after close");
    match err {
        crate::AgentError::InputQueueClosed { session_id: got } => {
            assert_eq!(
                got, session_id,
                "error must carry the originating session id"
            );
        }
        other => panic!("expected InputQueueClosed, got {other:?}"),
    }
}
