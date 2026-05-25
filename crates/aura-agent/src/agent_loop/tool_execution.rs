//! Tool result processing, caching, and build checks.

use std::collections::HashSet;

use crate::constants::{tool_result_cache_key, CACHEABLE_TOOLS};
use aura_reasoner::{ContentBlock, Message, ModelResponse, ToolResultContent};
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

use super::search_cache::normalized_search_key;
use super::streaming;
use super::{AgentLoop, LoopState};

fn is_cacheable(tool_name: &str) -> bool {
    CACHEABLE_TOOLS.contains(&tool_name)
}

pub(super) struct ExecutedTools {
    pub(super) tool_calls: Vec<ToolCallInfo>,
    pub(super) all_results: Vec<ToolCallResult>,
    pub(super) side_messages: Vec<String>,
    pub(super) blocked_ids: HashSet<String>,
    pub(super) cached_ids: HashSet<String>,
}

/// Handle `StopReason::ToolUse` — cache, execute, emit, stall-check.
///
/// Returns `true` if the loop should break.
pub(super) async fn handle_tool_use(
    agent: &AgentLoop,
    response: &ModelResponse,
    executor: &dyn AgentToolExecutor,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut LoopState,
) -> bool {
    let tools = match execute_and_cache_tools(agent, response, executor, state, event_tx).await {
        Some(t) => t,
        None => return true,
    };
    emit_and_log_results(event_tx, &tools);
    check_termination_conditions(event_tx, state, tools)
}

async fn execute_and_cache_tools(
    agent: &AgentLoop,
    response: &ModelResponse,
    executor: &dyn AgentToolExecutor,
    state: &mut LoopState,
    event_tx: Option<&Sender<AgentLoopEvent>>,
) -> Option<ExecutedTools> {
    let tool_calls = extract_tool_calls(response);
    if tool_calls.is_empty() {
        return None;
    }
    info!(
        tool_count = tool_calls.len(),
        "Processing tool_use stop reason"
    );
    for tc in &tool_calls {
        info!(
            tool_use_id = %tc.id,
            tool_name = %tc.name,
            is_write = helpers::is_write_tool(&tc.name),
            "Tool requested by model"
        );
    }

    let (cached_results, uncached_calls) = split_cached(
        &tool_calls,
        &state.tool_cache.exact,
        &state.tool_cache.fuzzy,
    );
    let cached_ids: HashSet<String> = cached_results
        .iter()
        .map(|r| r.tool_use_id.clone())
        .collect();
    info!(
        cached_count = cached_results.len(),
        execute_count = uncached_calls.len(),
        "Resolved cached vs executable tool calls"
    );

    let (executed_results, side_messages, blocked_ids) = if uncached_calls.is_empty() {
        (Vec::new(), Vec::new(), HashSet::new())
    } else {
        agent
            .process_tool_results(&uncached_calls, executor, state, event_tx)
            .await
    };

    update_cache(
        &mut state.tool_cache.exact,
        &mut state.tool_cache.fuzzy,
        &uncached_calls,
        &executed_results,
    );

    let mut all_results: Vec<ToolCallResult> = cached_results;
    all_results.extend(executed_results);

    Some(ExecutedTools {
        tool_calls,
        all_results,
        side_messages,
        blocked_ids,
        cached_ids,
    })
}

/// Maximum characters of the tool result body included in the
/// `Tool call completed` log line as `result_preview`. Only emitted on
/// errors so the operator can diagnose tool rejections (e.g. write_file
/// validation, task_done gate) without dumping every successful tool's
/// full output into `harness.log`. Sized to comfortably hold the
/// `task_done` rejection text (~298B) and most validation errors from
/// `aura-tools` while staying under any tracing field truncation limits.
const TOOL_ERROR_PREVIEW_LIMIT: usize = 1024;

fn emit_and_log_results(event_tx: Option<&Sender<AgentLoopEvent>>, tools: &ExecutedTools) {
    for r in &tools.all_results {
        let tool_name = tools
            .tool_calls
            .iter()
            .find(|t| t.id == r.tool_use_id)
            .map_or("unknown", |t| t.name.as_str());
        let source = if tools.cached_ids.contains(&r.tool_use_id) {
            "cache"
        } else if tools.blocked_ids.contains(&r.tool_use_id) {
            "blocked"
        } else {
            "executor"
        };
        if r.is_error {
            let preview = truncate_preview(&r.content, TOOL_ERROR_PREVIEW_LIMIT);
            info!(
                tool_use_id = %r.tool_use_id,
                tool_name = tool_name,
                is_write = helpers::is_write_tool(tool_name),
                is_error = r.is_error,
                stop_loop = r.stop_loop,
                source = source,
                result_len = r.content.len(),
                result_preview = preview.as_str(),
                "Tool call completed"
            );
        } else {
            info!(
                tool_use_id = %r.tool_use_id,
                tool_name = tool_name,
                is_write = helpers::is_write_tool(tool_name),
                is_error = r.is_error,
                stop_loop = r.stop_loop,
                source = source,
                result_len = r.content.len(),
                "Tool call completed"
            );
        }
    }
    emit_tool_results(event_tx, &tools.all_results, &tools.tool_calls);
}

/// Sanitise a tool error body for inline embedding in a `tracing` log
/// field: collapse whitespace, drop control characters, replace inner
/// double quotes (which would otherwise break naive `key="value"`
/// parsers like `infra/evals/external/bin/follow-harness-log.mjs`),
/// and clip to `limit` characters with an ASCII marker.
pub(super) fn truncate_preview(content: &str, limit: usize) -> String {
    let collapsed: String = content
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = collapsed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "'");
    if trimmed.chars().count() <= limit {
        trimmed
    } else {
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}...")
    }
}

pub(super) fn check_termination_conditions(
    _event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut LoopState,
    tools: ExecutedTools,
) -> bool {
    let should_stop = tools.all_results.iter().any(|r| r.stop_loop);

    for result in tools
        .all_results
        .iter()
        .filter(|r| r.kind == aura_core::ToolResultKind::CompactionStructural)
    {
        warn!(
            target: "compaction",
            tool_use_id = %result.tool_use_id,
            result_len = result.content.len(),
            "Rejected compacted/redacted tool input without incrementing consecutive errors"
        );
    }

    // Phase B of harness-v2.2: latch the cumulative
    // `had_any_file_write` / `task_done_completed` flags consulted by
    // `dispatch_stop_reason` when `dev_loop_completion_required` is on.
    //
    // `state.had_any_write` is set by `tool_pipeline::track_tool_effects`
    // earlier in the iteration on any successful path-carrying write
    // tool. Mirroring it here keeps the two flags in lockstep without
    // duplicating the write-detection logic. Phase E (apply_patch)
    // will add its own progress hook in the same place.
    if state.had_any_write {
        state.had_any_file_write = true;
    }
    // We derive `task_done_completed` from `stop_loop` on a `task_done`
    // tool call instead of plumbing `LoopState` into the executor's
    // `handle_task_done`. `stop_loop = true` + `is_error = false` is
    // already the handler's "all DoD gates passed" handshake; reading
    // it here keeps the handler signature small and avoids touching
    // every non-dev-loop caller of `handle_task_done`.
    let task_done_success = tools.tool_calls.iter().any(|tc| tc.name == "task_done")
        && tools
            .all_results
            .iter()
            .any(|r| !r.is_error && r.stop_loop);
    if task_done_success {
        state.task_done_completed = true;
    }

    push_tool_result_message_with_context(
        &mut state.messages,
        tools.all_results,
        tools.side_messages,
    );

    should_stop
}

fn extract_tool_calls(response: &ModelResponse) -> Vec<ToolCallInfo> {
    response
        .message
        .content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolUse { id, name, input } = block {
                Some(ToolCallInfo {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn split_cached(
    tool_calls: &[ToolCallInfo],
    cache: &std::collections::HashMap<String, String>,
    fuzzy_cache: &std::collections::HashMap<String, String>,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>) {
    let mut cached = Vec::new();
    let mut uncached = Vec::new();

    for tc in tool_calls {
        if !is_cacheable(&tc.name) {
            uncached.push(tc.clone());
            continue;
        }

        let exact_key = tool_result_cache_key(&tc.name, &tc.input);
        if let Some(hit) = cache.get(&exact_key) {
            info!(
                tool_use_id = %tc.id,
                tool_name = %tc.name,
                source = "cache:exact",
                "Tool call satisfied from cache"
            );
            cached.push(cached_tool_result(tc, hit.clone()));
            continue;
        }

        // Fall back to the normalized (fuzzy) index for
        // `search_code` / `find_files` only. Other cacheable tools
        // (`read_file`, `list_files`, `stat_file`) stay exact-only
        // because their keys already describe a single resource.
        if let Some(fkey) = normalized_search_key(&tc.name, &tc.input) {
            if let Some(hit) = fuzzy_cache.get(&fkey) {
                info!(
                    tool_use_id = %tc.id,
                    tool_name = %tc.name,
                    source = "cache:fuzzy",
                    "Tool call satisfied from fuzzy cache"
                );
                cached.push(cached_tool_result(tc, hit.clone()));
                continue;
            }
        }

        uncached.push(tc.clone());
    }

    (cached, uncached)
}

fn cached_tool_result(call: &ToolCallInfo, content: String) -> ToolCallResult {
    ToolCallResult {
        tool_use_id: call.id.clone(),
        content,
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    }
}

pub(super) fn update_cache(
    cache: &mut std::collections::HashMap<String, String>,
    fuzzy_cache: &mut std::collections::HashMap<String, String>,
    uncached: &[ToolCallInfo],
    executed: &[ToolCallResult],
) {
    let any_write = uncached.iter().any(|tc| {
        helpers::is_write_tool(&tc.name)
            && executed
                .iter()
                .any(|r| r.tool_use_id == tc.id && !r.is_error)
    });
    if any_write {
        cache.clear();
        fuzzy_cache.clear();
    }

    for r in executed {
        if let Some(tc) = uncached.iter().find(|t| t.id == r.tool_use_id) {
            if is_cacheable(&tc.name) && !r.is_error {
                let key = tool_result_cache_key(&tc.name, &tc.input);
                cache.insert(key, r.content.clone());
                if let Some(fkey) = normalized_search_key(&tc.name, &tc.input) {
                    fuzzy_cache.insert(fkey, r.content.clone());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event emission and message helpers
// ---------------------------------------------------------------------------

fn emit_tool_results(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    all_results: &[ToolCallResult],
    tool_calls: &[ToolCallInfo],
) {
    for r in all_results {
        let info = tool_calls.iter().find(|t| t.id == r.tool_use_id);
        let tool_name = info.map_or_else(String::new, |t| t.name.clone());
        // Emit `ToolCallCompleted` FIRST so downstream forwarders (the
        // aura-os-server dev-loop DoD gate in particular) see the
        // authoritative `{id, name, input, is_error}` frame before the
        // result text arrives. Carries the fully-parsed input so
        // consumers don't have to stitch it together from the earlier
        // streaming `ToolInputSnapshot` (which may be partial JSON).
        streaming::emit(
            event_tx,
            AgentLoopEvent::ToolCallCompleted {
                tool_use_id: r.tool_use_id.clone(),
                tool_name: tool_name.clone(),
                input: info.map_or(serde_json::Value::Null, |t| t.input.clone()),
                is_error: r.is_error,
            },
        );
        streaming::emit(
            event_tx,
            AgentLoopEvent::ToolResult {
                tool_use_id: r.tool_use_id.clone(),
                tool_name,
                content: r.content.clone(),
                is_error: r.is_error,
            },
        );
    }
}

/// Build a single user message with `tool_result` blocks first, followed by any
/// optional context text blocks.
///
/// Anthropic requires that every assistant `tool_use` is immediately paired by
/// `tool_result` blocks in the next user message. Keeping tool results first
/// avoids ambiguity from prepended warning/build text blocks.
pub(super) fn push_tool_result_message_with_context(
    messages: &mut Vec<Message>,
    results: Vec<ToolCallResult>,
    context_texts: Vec<String>,
) {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for r in results {
        blocks.push(ContentBlock::tool_result(
            &r.tool_use_id,
            ToolResultContent::text(r.content),
            r.is_error,
        ));
    }
    for text in context_texts {
        blocks.push(ContentBlock::Text { text });
    }

    if !blocks.is_empty() {
        messages.push(Message::new(aura_reasoner::Role::User, blocks));
    }
}
