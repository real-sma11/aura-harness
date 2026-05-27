//! Compaction-summary user-prompt rendering — agent-side half.
//!
//! The render-only template (system prompt, header literals,
//! numeric-input formatter) lives in
//! `aura_prompts::auxiliary::compaction`. The `Vec<aura_reasoner::Message>`
//! walker that produces the per-message text for the two body
//! sections has to stay in `aura-agent` because it depends on
//! `aura-reasoner`, which the prompts crate is forbidden from
//! pulling in.
//!
//! Net: every model-facing literal is still in `aura-prompts`; only
//! the per-message rendering bytes-for-bytes match the pre-Phase-2
//! inline implementation that used to live in
//! `prompts/auxiliary/compaction.rs`.

use aura_prompts::auxiliary::compaction::{
    build_compact_summary_user_prompt, CompactSummaryHeader,
};
use aura_reasoner::{ContentBlock, Message, ToolResultContent};

/// Render the user-channel prompt for the compaction-summary LLM
/// call.
#[must_use]
pub(super) fn render_user_prompt(input: &aura_compaction::SummaryInput) -> String {
    let header = CompactSummaryHeader {
        max_summary_chars: input.max_summary_chars,
        original_chars: input.original_chars,
        local_chars: input.local_chars,
        target_total_chars: input.target_total_chars,
    };
    let middle = render_message_block(&input.compactable_messages);
    let tail = render_message_block(&input.recent_tail);
    build_compact_summary_user_prompt(header, &middle, &tail)
}

fn render_message_block(messages: &[Message]) -> String {
    let mut out = String::new();
    for (idx, message) in messages.iter().enumerate() {
        out.push_str(&render_summary_message(idx, message));
    }
    out
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
    let max_block_chars = aura_config::PROMPT_COMPACTION_MAX_BLOCK_CHARS;
    if text.len() <= max_block_chars {
        text.to_string()
    } else {
        aura_compaction::truncate_content(text, max_block_chars, Some(2_000), Some(1_000))
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
        let prompt = render_user_prompt(&small_input());
        assert!(prompt.contains("Summarize the compactable middle history below"));
        assert!(prompt.contains("no more than about 1000 characters"));
        assert!(prompt.contains("Target total transcript chars after summary: 2000"));
        assert!(prompt.contains("## Compactable Middle History"));
        assert!(prompt.contains("## Recent Tail Kept Verbatim"));
        assert!(prompt.contains("first"));
        assert!(prompt.contains("latest"));
    }
}
