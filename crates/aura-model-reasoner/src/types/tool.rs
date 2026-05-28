use serde::{Deserialize, Serialize};

pub use aura_core::{CacheControl, ToolDefinition};

// ============================================================================
// Tool Choice
// ============================================================================

/// How the model should choose tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to use tools
    #[default]
    Auto,
    /// Model should not use any tools
    None,
    /// Model must use a tool
    Required,
    /// Model must use the specified tool
    Tool { name: String },
}

impl ToolChoice {
    /// Create a tool choice for a specific tool.
    #[must_use]
    pub fn tool(name: impl Into<String>) -> Self {
        Self::Tool { name: name.into() }
    }
}
