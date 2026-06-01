//! Build the per-turn [`AgentContextContents`] — the actual rendered
//! text for each static context bucket — alongside the token-count
//! [`crate::types::AgentContextBreakdown`].
//!
//! Lives next to (but separate from) `context.rs` to keep that already
//! large module from growing further. The single entry point
//! [`build_context_contents`] is called from
//! [`super::context::recompute_breakdown`] so the contents stay in sync
//! with the breakdown on every turn that reaches the compaction step.

use aura_config::CHARS_PER_TOKEN;
use aura_model_reasoner::ToolDefinition;

use super::AgentLoopConfig;
use crate::types::{AgentContextContents, AgentContextSegment};

/// Char-to-token conversion shared with `context.rs`. Mirrors the
/// `chars / CHARS_PER_TOKEN` heuristic so per-segment tokens line up
/// with the matching breakdown bucket.
fn chars_to_tokens(chars: usize) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (chars / CHARS_PER_TOKEN) as u64
    }
}

/// Render a single tool into readable bucket text: name, description,
/// then the pretty-printed JSON schema. A schema that fails to
/// serialize degrades to an empty body rather than panicking.
fn render_tool_text(tool: &ToolDefinition) -> String {
    let schema = serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default();
    format!("{}\n\n{}\n\n{}", tool.name, tool.description, schema)
}

/// One [`AgentContextSegment`] per tool in the effective surface.
fn build_tool_segments(effective_tools: &[ToolDefinition]) -> Vec<AgentContextSegment> {
    effective_tools
        .iter()
        .map(|tool| {
            let text = render_tool_text(tool);
            let tokens = chars_to_tokens(text.len());
            AgentContextSegment {
                label: tool.name.clone(),
                text,
                tokens,
            }
        })
        .collect()
}

/// Convert pre-rendered `(label, text)` pairs (skills, subagents) into
/// segments, stamping each with a token estimate. Shared by the skills
/// and subagents buckets so the two stay identical.
fn segments_from_pairs(pairs: &[(String, String)]) -> Vec<AgentContextSegment> {
    pairs
        .iter()
        .map(|(label, text)| AgentContextSegment {
            label: label.clone(),
            text: text.clone(),
            tokens: chars_to_tokens(text.len()),
        })
        .collect()
}

/// Build the full per-turn contents from the same sources as
/// [`crate::types::AgentContextBreakdown`].
///
/// `system_prompt` carries the FULL prompt (which already includes any
/// injected skill text); the skills bucket therefore only carries each
/// skill's label + summary, never the body, so the text is not
/// double-counted across buckets.
pub(super) fn build_context_contents(
    config: &AgentLoopConfig,
    effective_tools: &[ToolDefinition],
) -> AgentContextContents {
    let system_prompt = if config.system_prompt.is_empty() {
        None
    } else {
        Some(config.system_prompt.clone())
    };
    AgentContextContents {
        system_prompt,
        tools: build_tool_segments(effective_tools),
        skills: segments_from_pairs(&config.skills_segments),
        subagents: segments_from_pairs(&config.subagents_segments),
        mcp: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::build_context_contents;
    use crate::agent_loop::AgentLoopConfig;
    use aura_model_reasoner::ToolDefinition;

    fn dummy_tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            description,
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        )
    }

    #[test]
    fn system_prompt_is_some_when_configured() {
        let config = AgentLoopConfig {
            system_prompt: "you are a helpful agent".to_string(),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let contents = build_context_contents(&config, &[]);
        assert_eq!(
            contents.system_prompt.as_deref(),
            Some("you are a helpful agent")
        );
    }

    #[test]
    fn system_prompt_is_none_when_empty() {
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let contents = build_context_contents(&config, &[]);
        assert!(contents.system_prompt.is_none());
    }

    #[test]
    fn tool_segments_carry_name_and_nonzero_tokens() {
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let tools = vec![
            dummy_tool("read_file", "Read a file from disk."),
            dummy_tool("write_file", "Write a file to disk."),
        ];
        let contents = build_context_contents(&config, &tools);
        assert_eq!(contents.tools.len(), 2);
        assert_eq!(contents.tools[0].label, "read_file");
        assert!(contents.tools[0].text.contains("Read a file from disk."));
        assert!(
            contents.tools[0].tokens > 0,
            "tool segment should carry a non-zero token estimate"
        );
    }

    #[test]
    fn skill_and_subagent_segments_come_from_config_pairs() {
        let config = AgentLoopConfig {
            skills_segments: vec![("deploy".to_string(), "Deploy the app".to_string())],
            subagents_segments: vec![(
                "explore".to_string(),
                "Read-only exploration subagent".to_string(),
            )],
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let contents = build_context_contents(&config, &[]);
        assert_eq!(contents.skills.len(), 1);
        assert_eq!(contents.skills[0].label, "deploy");
        assert_eq!(contents.skills[0].text, "Deploy the app");
        assert_eq!(contents.subagents.len(), 1);
        assert_eq!(contents.subagents[0].label, "explore");
        assert!(contents.mcp.is_empty(), "mcp bucket is reserved (empty)");
    }
}
