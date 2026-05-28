use serde::{Deserialize, Serialize};

pub use aura_core::ToolResultContent;

// ============================================================================
// Role and Content Types
// ============================================================================

/// Role in conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// User message
    User,
    /// Assistant (model) message
    Assistant,
}

/// Content block in a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text content
    Text { text: String },

    /// Thinking content (extended thinking from Claude)
    /// When echoing back to the API, the signature must be included.
    Thinking {
        thinking: String,
        /// Signature for the thinking block - required when echoing back to API
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// Inline image content (user messages only).
    Image { source: ImageSource },

    /// Model requesting tool use (assistant only)
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// Result of tool execution (user only, in response to `tool_use`)
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        is_error: bool,
    },
}

/// Source data for an inline image content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    /// Source type (e.g., `"base64"`).
    #[serde(rename = "type")]
    pub source_type: String,
    /// Media type (e.g., `"image/png"`, `"image/jpeg"`).
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
}

impl ContentBlock {
    /// Create a text content block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Create a tool use content block.
    #[must_use]
    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    /// Create a tool result content block.
    #[must_use]
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: ToolResultContent,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
            is_error,
        }
    }

    /// Get the text content if this is a text block.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Check if this is a tool use block.
    #[must_use]
    pub const fn is_tool_use(&self) -> bool {
        matches!(self, Self::ToolUse { .. })
    }
}
