//! Tool-surface compaction helpers.

use aura_reasoner::ToolDefinition;

/// Report returned after compacting a tool surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSurfaceReport {
    /// Number of tool definitions processed.
    pub tool_count: usize,
    /// Estimated surface size before compaction.
    pub before_chars: usize,
    /// Estimated surface size after compaction.
    pub after_chars: usize,
}

/// Estimate the on-wire size of a `ToolDefinition`.
#[must_use]
pub fn tool_definition_chars(tool: &ToolDefinition) -> usize {
    let schema_chars = serde_json::to_string(&tool.input_schema).map_or(0, |s| s.len());
    tool.name.len() + tool.description.len() + schema_chars
}

/// Sum the per-tool char estimate across an effective tool surface.
#[must_use]
pub fn tools_chars(tools: &[ToolDefinition]) -> usize {
    tools.iter().map(tool_definition_chars).sum()
}

/// Strip property descriptions from tool definitions to reduce token usage.
pub fn compact_tools(tools: &mut [ToolDefinition]) {
    for tool in tools {
        if let Some(props) = tool.input_schema.get_mut("properties") {
            if let Some(obj) = props.as_object_mut() {
                for (_, prop_schema) in obj.iter_mut() {
                    if let Some(inner) = prop_schema.as_object_mut() {
                        inner.remove("description");
                    }
                }
            }
        }
    }
}

/// Compact the effective tool surface and report the size change.
pub fn compact_tool_surface(tools: &mut [ToolDefinition]) -> ToolSurfaceReport {
    let before_chars = tools_chars(tools);
    compact_tools(tools);
    let after_chars = tools_chars(tools);
    ToolSurfaceReport {
        tool_count: tools.len(),
        before_chars,
        after_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_tools_strips_descriptions() {
        let mut tools = vec![ToolDefinition::new(
            "test",
            "A tool",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file path"
                    }
                }
            }),
        )];
        compact_tools(&mut tools);
        let props = tools[0].input_schema["properties"]["path"]
            .as_object()
            .unwrap();
        assert!(!props.contains_key("description"));
        assert!(props.contains_key("type"));
    }
}
