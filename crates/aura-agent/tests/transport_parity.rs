//! Phase 4 keystone: transport parity test.
//!
//! This test is the structural keystone that proves the buffered
//! (`complete_with_streaming`) and streaming pump
//! (`stream_pump::run_stream_pump`) sampling paths produce
//! byte-identical post-iteration state for the same input. It is
//! the gate that unlocks the Phase 7 deletion of the legacy
//! buffered streaming path.
//!
//! The test runs the same scripted [`MockProvider`] script (a
//! two-iteration trace: tool_use then text + EndTurn) through both
//! transports by flipping
//! [`aura_agent::AgentLoopConfig::use_stream_pump`] between runs.
//! Everything else — the system prompt, tool definitions, executor
//! results, message seed, etc. — is identical. The two
//! [`aura_agent::AgentLoopResult`]s are then compared field by
//! field; any drift would mean the dual paths produce divergent
//! transcripts, which is precisely the regression Phase 4 is meant
//! to prevent.
//!
//! Fields compared (mirroring the plan's acceptance criteria):
//!
//! - `messages` (full final transcript, byte-equal)
//! - `iterations`
//! - `total_text` / `total_thinking`
//! - `total_input_tokens` / `total_output_tokens`
//! - `total_cache_creation_input_tokens` /
//!   `total_cache_read_input_tokens`
//! - `estimated_context_tokens`
//! - `context_breakdown` (per-bucket sub-totals)
//! - `file_changes`
//! - terminal flags (`timed_out`, `insufficient_credits`, `stalled`,
//!   `llm_error`)

use async_trait::async_trait;

use aura_agent::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};
use aura_agent::{AgentLoop, AgentLoopConfig};
use aura_reasoner::{
    ContentBlock, Message, MockProvider, MockResponse, StopReason, ToolDefinition, Usage,
};

/// Scripted executor that returns canned [`ToolCallResult`]s in the
/// same submission order they were issued. Cloneable so the test
/// can hand one to each transport run without sharing internal
/// state.
#[derive(Clone)]
struct ScriptedExecutor {
    results: Vec<ToolCallResult>,
}

#[async_trait]
impl AgentToolExecutor for ScriptedExecutor {
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

/// Build the same two-iteration scripted [`MockProvider`] every time
/// so each transport run is fed an identical model fixture.
///
/// Iteration 1 — text + tool_use (`StopReason::ToolUse`). The
/// thinking-text-tool_use ordering matches the pump's
/// `synthesize_response` output so the post-iteration `messages` vec
/// is byte-equal on both transports.
///
/// Iteration 2 — plain text reply (`StopReason::EndTurn`). Drives
/// the loop to terminate naturally after one tool round-trip.
fn fixture_provider() -> MockProvider {
    let first = MockResponse {
        stop_reason: StopReason::ToolUse,
        content: vec![
            ContentBlock::text("Looking up the file."),
            ContentBlock::tool_use(
                "toolu_1",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            ),
        ],
        usage: Usage::new(120, 40),
    };
    let second = MockResponse {
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::text("Done.")],
        usage: Usage::new(150, 25),
    };
    MockProvider::new()
        .with_response(first)
        .with_response(second)
}

fn read_file_tool() -> ToolDefinition {
    ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
    )
}

fn parity_config(use_stream_pump: bool) -> AgentLoopConfig {
    AgentLoopConfig {
        system_prompt: "transport-parity test agent".to_string(),
        // Pin the pump toggle to the parametrised value; everything
        // else stays at the per-agent default so the only knob the
        // test moves between runs is the transport selector.
        use_stream_pump,
        ..AgentLoopConfig::for_agent("claude-test-model")
    }
}

/// Drive one iteration through the selected transport.
async fn run_one_iteration(use_stream_pump: bool) -> aura_agent::AgentLoopResult {
    let executor = ScriptedExecutor {
        results: vec![ToolCallResult::success("placeholder", "fn main() {}")],
    };
    let provider = fixture_provider();
    let agent = AgentLoop::new(parity_config(use_stream_pump));
    let messages = vec![Message::user("Tell me what's in src/lib.rs.")];
    let tools = vec![read_file_tool()];

    agent
        .run(&provider, &executor, messages, tools)
        .await
        .expect("scripted run must succeed")
}

#[tokio::test]
async fn transport_parity_same_post_iteration_state() {
    let buffered = run_one_iteration(false).await;
    let streamed = run_one_iteration(true).await;

    // --- High-level loop counters and terminal flags ---
    assert_eq!(
        buffered.iterations, streamed.iterations,
        "iteration counters must match across transports"
    );
    assert_eq!(buffered.timed_out, streamed.timed_out);
    assert_eq!(buffered.insufficient_credits, streamed.insufficient_credits);
    assert_eq!(buffered.stalled, streamed.stalled);
    assert_eq!(buffered.llm_error, streamed.llm_error);

    // --- Token accounting (the pump's accumulator and the buffered
    // path's response.usage feed into the same accumulate_response
    // step, so the per-iteration totals must roll up identically). ---
    assert_eq!(buffered.total_input_tokens, streamed.total_input_tokens);
    assert_eq!(buffered.total_output_tokens, streamed.total_output_tokens);
    assert_eq!(
        buffered.total_cache_creation_input_tokens,
        streamed.total_cache_creation_input_tokens
    );
    assert_eq!(
        buffered.total_cache_read_input_tokens,
        streamed.total_cache_read_input_tokens
    );
    assert_eq!(
        buffered.estimated_context_tokens,
        streamed.estimated_context_tokens
    );

    // --- Context breakdown (per-bucket compaction telemetry). ---
    assert_eq!(buffered.context_breakdown, streamed.context_breakdown);

    // --- Accumulated assistant text / thinking. ---
    assert_eq!(buffered.total_text, streamed.total_text);
    assert_eq!(buffered.total_thinking, streamed.total_thinking);

    // --- File changes (no writes in this scenario, but the equality
    // check still pins the parity for follow-up scenarios). ---
    assert_eq!(buffered.file_changes, streamed.file_changes);

    // --- Full transcript byte-equality (the keystone assertion). ---
    assert_eq!(
        buffered.messages.len(),
        streamed.messages.len(),
        "message vec length must match",
    );
    // `ContentBlock` does not implement `PartialEq` (the embedded
    // `serde_json::Value` blocks make a structural derive non-trivial
    // and the upstream crate has chosen not to land it). Serialise
    // each message to JSON for the parity check — that is also how
    // the loop hands the message off to the model on the next turn,
    // so byte-equal JSON is a stricter guarantee than block-by-block
    // structural equality.
    for (idx, (b, s)) in buffered
        .messages
        .iter()
        .zip(streamed.messages.iter())
        .enumerate()
    {
        let b_json = serde_json::to_value(b).expect("messages must serialise (buffered)");
        let s_json = serde_json::to_value(s).expect("messages must serialise (streamed)");
        assert_eq!(b_json, s_json, "message[{idx}] drifted between transports");
    }
}
