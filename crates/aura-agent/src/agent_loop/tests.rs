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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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
        ..AgentLoopConfig::default()
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

    let config = AgentLoopConfig::default();
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

    let config = AgentLoopConfig::default();
    let mut state = super::LoopState::new(&config, vec![]);
    state.submit_plan_called = true;
    assert_eq!(
        state.compute_thinking_effort(&config, 2),
        ThinkingEffort::Low
    );
}

/// After a continuation steering message was just injected by the
/// Phase 1.B runtime (`consecutive_no_write > 0`), drop to `Low`.
/// The harness is already pushing the model forward — extra
/// deliberation budget would feed the same read-loop the
/// continuation prompt is trying to break.
#[test]
fn effort_low_after_continuation_injected() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig::default();
    let mut state = super::LoopState::new(&config, vec![]);
    state.continuation.consecutive_no_write = 1;
    assert_eq!(
        state.compute_thinking_effort(&config, 4),
        ThinkingEffort::Low
    );
}

/// Non-iteration-0 iterations without writes, plans, or continuation
/// pressure default to `Medium`. Confirms the policy doesn't
/// silently fall to `Low` without one of the explicit triggers.
#[test]
fn effort_medium_default_after_iteration_zero() {
    use aura_reasoner::ThinkingEffort;

    let config = AgentLoopConfig::default();
    let state = super::LoopState::new(&config, vec![]);
    assert_eq!(
        state.compute_thinking_effort(&config, 3),
        ThinkingEffort::Medium
    );
}

/// `build_request` only opts into the explicit effort knob for
/// dev-loop callers. Chat / generic callers leave `thinking_effort`
/// `None` so they keep the legacy `max_tokens > 2048` auto-enable
/// path. This is the backwards-compatibility seam called out in the
/// commit message.
#[test]
fn build_request_emits_thinking_effort_only_for_dev_loop() {
    let chat_config = AgentLoopConfig {
        dev_loop_completion_required: false,
        ..AgentLoopConfig::default()
    };
    let chat_state = super::LoopState::new(&chat_config, vec![]);
    let chat_req = chat_state
        .build_request(&chat_config, &[], 2)
        .expect("build_request must succeed for chat");
    assert!(
        chat_req.thinking_effort.is_none(),
        "chat callers must NOT opt into the new effort knob (preserves legacy behaviour)"
    );

    let dev_config = AgentLoopConfig {
        dev_loop_completion_required: true,
        disable_thinking_iteration_0: true,
        ..AgentLoopConfig::default()
    };
    let dev_state = super::LoopState::new(&dev_config, vec![]);
    let dev_req = dev_state
        .build_request(&dev_config, &[], 0)
        .expect("build_request must succeed for dev-loop");
    assert!(
        dev_req.thinking_effort.is_some(),
        "dev-loop callers must opt into the explicit effort knob"
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

