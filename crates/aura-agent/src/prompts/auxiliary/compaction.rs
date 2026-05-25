//! Compaction summary LLM-call prompts.
//!
//! When the conversation history outgrows the model's context window
//! the agent loop fires an auxiliary LLM call that asks the model to
//! summarise the compactable middle of the transcript. PR A removed
//! the long-unused `CONTEXT_SUMMARY_SYSTEM_PROMPT` constant; PR D
//! pulls the *live* system + user strings the call uses out of
//! [`crate::agent_loop`] and into this submodule so every
//! model-facing string the harness emits lives under
//! `crates/aura-agent/src/prompts/`.
//!
//! Inputs / outputs match the pre-PR-D inline call site byte-for-byte:
//!
//! - [`COMPACTION_SUMMARY_SYSTEM_PROMPT`] is the system string passed
//!   to [`aura_reasoner::ModelRequest::builder`].
//! - [`build_compact_summary_user_prompt`] renders the user message
//!   the request carries, including the compactable-middle preview
//!   and a verbatim recent tail.

use aura_reasoner::{ContentBlock, Message, ToolResultContent};

/// System prompt for the compaction summary LLM call.
///
/// Lifted verbatim from the pre-PR-D inline literal in
/// `agent_loop::AgentLoop::build_summary_request`.
pub const COMPACTION_SUMMARY_SYSTEM_PROMPT: &str =
    "Summarize compacted conversation history for a coding agent. \
     Preserve concrete decisions, files, tool outcomes, errors, and unresolved tasks. \
     Do not invent facts.";

/// Build the user-channel prompt body for the compaction summary
/// LLM call.
///
/// Output bytes match the pre-PR-D `agent_loop::compact_summary_prompt`
/// implementation; the move is purely organisational. The prompt
/// targets `input.max_summary_chars` and embeds both the compactable
/// middle history (the slice the loop is asking the model to
/// compress) and the recent tail kept verbatim (so the summary call
/// sees the context the next live turn will see).
#[must_use]
pub fn build_compact_summary_user_prompt(input: &aura_compaction::SummaryInput) -> String {
    let mut prompt = format!(
        "Summarize the compactable middle history below into no more than about {} characters.\n\
         The live transcript was {} chars before local compaction and {} chars after local compaction.\n\
         Target total transcript chars after summary: {}.\n\
         Keep exact file paths, tool names, important outputs, decisions, and unresolved errors.\n\n\
         ## Compactable Middle History\n",
        input.max_summary_chars,
        input.original_chars,
        input.local_chars,
        input.target_total_chars,
    );

    for (idx, message) in input.compactable_messages.iter().enumerate() {
        prompt.push_str(&render_summary_message(idx, message));
    }

    prompt.push_str("\n## Recent Tail Kept Verbatim\n");
    for (idx, message) in input.recent_tail.iter().enumerate() {
        prompt.push_str(&render_summary_message(idx, message));
    }
    prompt
}

fn render_summary_message(idx: usize, message: &Message) -> String {
    let mut rendered = format!("\n### Message {idx} ({:?})\n", message.role);
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                rendered.push_str("text:\n");
                rendered.push_str(&truncate_for_summary_prompt(text));
                rendered.push('\n');
            }
            ContentBlock::Thinking { thinking, .. } => {
                rendered.push_str("thinking:\n");
                rendered.push_str(&truncate_for_summary_prompt(thinking));
                rendered.push('\n');
            }
            ContentBlock::ToolUse { id, name, input } => {
                rendered.push_str(&format!("tool_use id={id} name={name} input="));
                rendered.push_str(
                    &serde_json::to_string(input)
                        .unwrap_or_else(|_| "<unserializable>".to_string()),
                );
                rendered.push('\n');
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                rendered.push_str(&format!(
                    "tool_result id={tool_use_id} is_error={is_error}:\n"
                ));
                match content {
                    ToolResultContent::Text(text) => {
                        rendered.push_str(&truncate_for_summary_prompt(text));
                    }
                    ToolResultContent::Json(value) => rendered.push_str(
                        &serde_json::to_string(value)
                            .unwrap_or_else(|_| "<unserializable>".to_string()),
                    ),
                }
                rendered.push('\n');
            }
            ContentBlock::Image { .. } => {
                rendered.push_str("image: [omitted]\n");
            }
        }
    }
    rendered
}

fn truncate_for_summary_prompt(text: &str) -> String {
    const MAX_BLOCK_CHARS: usize = 4_000;
    if text.len() <= MAX_BLOCK_CHARS {
        text.to_string()
    } else {
        aura_compaction::truncate_content(text, MAX_BLOCK_CHARS, Some(2_000), Some(1_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_input() -> aura_compaction::SummaryInput {
        aura_compaction::SummaryInput {
            compactable_messages: vec![Message::user("first")],
            recent_tail: vec![Message::assistant("latest")],
            max_summary_chars: 1_000,
            original_chars: 5_000,
            local_chars: 4_000,
            target_total_chars: 2_000,
            compact_start: 0,
            compact_end: 1,
        }
    }

    #[test]
    fn user_prompt_includes_targets_and_sections() {
        let prompt = build_compact_summary_user_prompt(&small_input());
        assert!(prompt.contains("Summarize the compactable middle history below"));
        assert!(prompt.contains("no more than about 1000 characters"));
        assert!(prompt.contains("Target total transcript chars after summary: 2000"));
        assert!(prompt.contains("## Compactable Middle History"));
        assert!(prompt.contains("## Recent Tail Kept Verbatim"));
    }

    #[test]
    fn system_prompt_is_stable() {
        assert!(COMPACTION_SUMMARY_SYSTEM_PROMPT.starts_with("Summarize compacted conversation"));
        assert!(COMPACTION_SUMMARY_SYSTEM_PROMPT.contains("Do not invent facts"));
    }
}
