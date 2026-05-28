use super::content::{ContentBlock, Role, ToolResultContent};
use serde::{Deserialize, Serialize};

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role of the message sender
    pub role: Role,
    /// Content blocks in the message
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Create a new message.
    #[must_use]
    pub const fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    /// Create a user message with text content.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create an assistant message with text content.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create a user message with tool results.
    #[must_use]
    pub fn tool_results(results: Vec<(String, ToolResultContent, bool)>) -> Self {
        Self {
            role: Role::User,
            content: results
                .into_iter()
                .map(|(id, content, is_error)| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    is_error,
                })
                .collect(),
        }
    }

    /// Get all text content concatenated.
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// Get all tool use blocks.
    #[must_use]
    pub fn tool_uses(&self) -> Vec<&ContentBlock> {
        self.content.iter().filter(|b| b.is_tool_use()).collect()
    }

    /// Check if this message contains tool use.
    #[must_use]
    pub fn has_tool_use(&self) -> bool {
        self.content.iter().any(ContentBlock::is_tool_use)
    }
}
