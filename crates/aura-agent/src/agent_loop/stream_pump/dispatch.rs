//! Post-pump dispatch tail for the streaming sampling path
//! (Layer E.3 / E.4).
//!
//! Mirrors the buffered path's
//! [`super::super::AgentLoop::dispatch_stop_reason`] for pre-executed
//! tool batches: the pump already ran every tool (with the per-run
//! cache + per-tool timeout overlay) so this layer only needs to
//! latch write side-effects, run auto-build, and push the
//! tool_result-bearing user message in submission order so the
//! Anthropic `tool_use` / `tool_result` adjacency contract stays
//! intact.

use aura_reasoner::ModelResponse;
use tokio::sync::mpsc::Sender;

use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

use super::emit_event;
use super::AgentLoopConfig;

/// Post-pump dispatcher for the streaming sampling path.
///
/// Returns `true` when the sampling loop should break (terminal stop
/// reason or `stop_loop = true` tool result). Mirrors the buffered
/// path's contract so the sampling driver can fold this bit into
/// `SamplingRequestResult::needs_follow_up` without branching on the
/// pump-vs-buffered split.
pub(in crate::agent_loop) async fn dispatch_streamed_response(
    agent: &super::super::AgentLoop,
    executor: &dyn AgentToolExecutor,
    response: &ModelResponse,
    tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut super::super::LoopState,
) -> bool {
    // Codex parity: `EndTurn` / `StopSequence` always terminate the
    // loop. The model owns the exit signal; the harness no longer
    // intercepts empty terminations.
    let _ = agent;
    match response.stop_reason {
        aura_reasoner::StopReason::EndTurn | aura_reasoner::StopReason::StopSequence => true,
        aura_reasoner::StopReason::MaxTokens => {
            !super::super::iteration::handle_max_tokens(&agent.config, response, state)
        }
        aura_reasoner::StopReason::ToolUse => {
            handle_streamed_tool_use(&agent.config, executor, tool_results, event_tx, state).await
        }
    }
}

/// Streaming-pump analog of `tool_execution::handle_tool_use` that
/// consumes pre-executed [`ToolCallResult`]s instead of re-invoking
/// the executor. Emits per-result events, runs the auto-build
/// post-write side-step, and appends the `tool_result`-bearing user
/// message. Returns `true` when the sampling loop should break (the
/// buffered path's contract).
///
/// Layer E.4: tool-result caching now happens inside the pump's
/// `driver::drive_stream` (per-`OutputItemDone` lookup against
/// `state.tool_cache`) and auto-build runs here on each successful
/// write, mirroring the buffered path's
/// `tool_pipeline::process_tool_results` behaviour. This closes the
/// parity gap that kept the pump opt-in pre-E.4.
async fn handle_streamed_tool_use(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    tool_results: Vec<(ToolCallInfo, ToolCallResult)>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut super::super::LoopState,
) -> bool {
    if tool_results.is_empty() {
        // Empty tool batch on a `ToolUse` stop is the terminal case
        // — model said "more work" but emitted no actual tool calls,
        // so there is nothing to dispatch and nothing to wait on.
        // Mirrors the buffered path's
        // `tool_execution::handle_tool_use` contract where
        // `execute_and_cache_tools` returning `None` (empty
        // `extract_tool_calls`) folds straight into "break the
        // sampling loop". Returning `false` here would spin the
        // loop on a no-op response.
        //
        // Phase 3 made this branch reachable for the
        // `MaxTokens`-without-pending-tools case too, after the
        // `synthesize_response` mapping shifted that scenario from
        // `MaxTokens` to `ToolUse` (see `synthesize.rs` docstring).
        // Both interpretations now break here, preserving the
        // `MaxTokens` pre-Phase-3 behaviour while landing the more
        // accurate `ToolUse` classification.
        return true;
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
    let any_write_success = super::super::tool_pipeline::track_tool_effects_public(
        &tool_calls,
        &results,
        &mut state.result,
        &mut state.exploration_state,
        &mut state.had_any_write,
        &mut state.turn_diff,
        Some(&mut state.repeated_read_tracker),
        Some(&mut state.session_read_paths),
        Some(&mut state.read_after_write_allowances),
    );
    if state.had_any_write {
        state.had_any_file_write = true;
    }

    // Layer E.4: auto-build after a successful write through the
    // pump. Mirrors the buffered path's
    // `tool_pipeline::process_tool_results` step so the dev-loop's
    // build feedback loop fires on the pump path too. The build
    // output is appended to the trailing tool_result-bearing user
    // message via `push_tool_result_message_with_context`, the same
    // adapter the buffered path uses.
    let mut side_messages: Vec<String> = Vec::new();
    if any_write_success && state.build_cooldown == 0 {
        if let Some(build_text) = super::super::tool_pipeline::run_auto_build_public(
            config,
            executor,
            &mut state.build_cooldown,
            state.build_baseline.as_ref(),
        )
        .await
        {
            side_messages.push(build_text);
        }
    }

    for (call, result) in tool_calls.iter().zip(results.iter()) {
        // Emit the same `ToolCallCompleted` + `ToolResult` pair the
        // buffered path emits so downstream forwarders see a
        // consistent event sequence regardless of which sampling
        // path produced the result.
        emit_event(
            event_tx,
            AgentLoopEvent::ToolCallCompleted {
                tool_use_id: result.tool_use_id.clone(),
                tool_name: call.name.clone(),
                input: call.input.clone(),
                is_error: result.is_error,
            },
        );
        emit_event(
            event_tx,
            AgentLoopEvent::ToolResult {
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
    super::super::tool_execution::push_tool_result_message_with_context(
        &mut state.messages,
        results,
        side_messages,
    );
    should_stop
}
