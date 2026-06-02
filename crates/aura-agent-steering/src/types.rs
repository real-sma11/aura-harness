//! Data shapes the steering evaluators observe.
//!
//! These types were relocated from `aura-agent::types` in Phase 6a so
//! the steering crate can sit BELOW the agent loop in the layer
//! order. `aura-agent` re-exports them under their historical paths
//! (`crate::types::ToolCallInfo`, `crate::types::ToolCallResult`,
//! `crate::types::FileChange`, `crate::types::FileChangeKind`) so
//! existing call sites are unchanged.

use serde::{Deserialize, Serialize};

/// Normalized file mutation kind for turn-level reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Create,
    Modify,
    Delete,
}

/// A single file mutation observed during execution.
///
/// `lines_added` / `lines_removed` are populated by tools that can
/// compute a diff cheaply (currently `edit_file`, which has both
/// `old_text` and `new_text` in its input). Tools that can't (e.g.
/// `write_file` without pre-content, `delete_file` after the file is
/// gone) leave the counts at 0 — downstream consumers must treat 0
/// as "unknown", not "no change".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Information about a tool call to be executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    /// Tool use ID from the model.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool arguments as JSON.
    pub input: serde_json::Value,
}

/// Result of executing a single tool call.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    /// Tool use ID.
    pub tool_use_id: String,
    /// Result content (text or error message).
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Machine-readable result classification.
    pub kind: aura_core_types::ToolResultKind,
    /// When true, the loop terminates after processing all results in
    /// this batch. Used by engine tools like `task_done` to signal
    /// task completion.
    pub stop_loop: bool,
    /// File mutations performed by this tool call, if known.
    pub file_changes: Vec<FileChange>,
    /// Optional rendered image (base64 + media type) produced by
    /// computer-use / vision tools (e.g. the `computer` tool). `None`
    /// for text-only tools. Threaded from
    /// [`aura_core_types::ToolResult::image`] so the agent loop can
    /// replay the screenshot to the model and the outbound wire can
    /// carry it to aura-os.
    pub image: Option<aura_core_types::ToolResultImage>,
}

impl ToolCallResult {
    /// Create a successful result.
    #[must_use]
    pub fn success(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: false,
            kind: aura_core_types::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        }
    }

    /// Create an error result.
    #[must_use]
    pub fn error(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
            kind: aura_core_types::ToolResultKind::AgentError,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        }
    }

    /// Create a structural compaction/redaction error result.
    #[must_use]
    pub fn compaction_structural(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
            kind: aura_core_types::ToolResultKind::CompactionStructural,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        }
    }

    /// Attach file-change metadata to a tool result.
    #[must_use]
    pub fn with_file_changes(mut self, file_changes: Vec<FileChange>) -> Self {
        self.file_changes = file_changes;
        self
    }

    /// Attach a rendered image (base64 + media type) to this result.
    #[must_use]
    pub fn with_image(mut self, image: Option<aura_core_types::ToolResultImage>) -> Self {
        self.image = image;
        self
    }
}
