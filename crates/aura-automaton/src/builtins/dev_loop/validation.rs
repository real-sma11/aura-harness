//! Post-execution validation: turning a [`TaskExecutionResult`] into
//! either a forward-progress signal or a structured `NeedsDecomposition`
//! hint the orchestrator can consume.
//!
//! Kept separate from `aggregate.rs` because the two abstractions answer
//! different questions: `TaskAggregate` summarises file-change evidence
//! used by the commit gate, whereas `validate_execution` decides whether
//! the task should be retried, decomposed, or surfaced as a hard failure.

use std::collections::{HashMap, HashSet};

use aura_agent::agent_runner::TaskExecutionResult;
use aura_reasoner::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};

use crate::error::AutomatonError;

/// Structured hint attached to a `NeedsDecomposition` outcome so the
/// orchestrator (Phase 3, in aura-os) can auto-split a task that reached
/// implementation phase but produced no file operations. Empty/None fields
/// are expected when the validator cannot reliably recover the context.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecompositionHint {
    /// Unique paths the agent attempted to `write_file` / `edit_file`
    /// without ever producing a non-error `tool_result`.
    pub failed_paths: Vec<String>,
    /// Name of the most recent assistant-side tool_use block, if any.
    pub last_pending_tool_name: Option<String>,
    /// Short JSON summary of that tool_use's input (via
    /// `aura_compaction::summarize_write_input` when applicable).
    pub last_pending_tool_input_summary: Option<String>,
}

/// Validate an agent-task execution result. Returns:
/// - `Ok(exec)` when the task produced file ops or explicitly declared
///   no-changes-needed.
/// - `Err(AutomatonError::NeedsDecomposition { hint })` when the task
///   reached the implementing phase but produced no file ops — the caller
///   (or the Phase 3 orchestrator in aura-os) can consume the hint to
///   auto-split and retry.
/// - `Err(AutomatonError::AgentExecution(..))` for the classic
///   "completed-without-changes" case that never reached implementing.
pub(crate) fn validate_execution(
    exec: TaskExecutionResult,
) -> Result<TaskExecutionResult, AutomatonError> {
    if !exec.file_ops.is_empty() || exec.no_changes_needed {
        return Ok(exec);
    }

    if exec.reached_implementing {
        let hint = build_decomposition_hint(&exec.messages);
        return Err(AutomatonError::NeedsDecomposition { hint });
    }

    Err(AutomatonError::AgentExecution(
        "task completed without any file operations — completion not verified".into(),
    ))
}

/// Extract a best-effort `DecompositionHint` from the final message history
/// of a task that reached implementation phase without any file ops.
///
/// `failed_paths` = unique paths from write_file/edit_file tool_use blocks
/// whose tool_use id never produced a non-error tool_result.
/// `last_pending_tool_name` = name of the last ToolUse in the most recent
/// assistant message.
/// `last_pending_tool_input_summary` = short summary via
/// `aura_compaction::summarize_write_input` (when it applies) or the
/// raw JSON truncated to a reasonable length.
pub(crate) fn build_decomposition_hint(messages: &[Message]) -> DecompositionHint {
    if messages.is_empty() {
        return DecompositionHint::default();
    }

    let mut tool_uses: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    let mut successful_ids: HashSet<String> = HashSet::new();

    for msg in messages {
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        tool_uses.insert(id.clone(), (name.clone(), input.clone()));
                    }
                }
            }
            Role::User => {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = block
                    {
                        if !*is_error {
                            successful_ids.insert(tool_use_id.clone());
                        }
                    }
                }
            }
        }
    }

    let mut failed_paths: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();
    for (id, (name, input)) in &tool_uses {
        if successful_ids.contains(id) {
            continue;
        }
        if !matches!(name.as_str(), "write_file" | "edit_file") {
            continue;
        }
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            if seen_paths.insert(path.to_string()) {
                failed_paths.push(path.to_string());
            }
        }
    }

    let (last_pending_tool_name, last_pending_tool_input_summary) = last_pending_tool_use(messages);

    DecompositionHint {
        failed_paths,
        last_pending_tool_name,
        last_pending_tool_input_summary,
    }
}

fn last_pending_tool_use(messages: &[Message]) -> (Option<String>, Option<String>) {
    let last_assistant = messages.iter().rev().find(|m| m.role == Role::Assistant);
    let Some(msg) = last_assistant else {
        return (None, None);
    };
    let last_tool_use = msg.content.iter().rev().find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
        _ => None,
    });
    let Some((name, input)) = last_tool_use else {
        return (None, None);
    };

    let summary = aura_compaction::summarize_write_input(&name, &input)
        .and_then(|v| serde_json::to_string(&v).ok())
        .or_else(|| serde_json::to_string(&input).ok())
        .map(|s| truncate_summary(&s, 240));

    (Some(name), summary)
}

fn truncate_summary(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}
