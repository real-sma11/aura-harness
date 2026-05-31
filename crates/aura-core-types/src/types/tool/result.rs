//! Tool execution result envelope returned to the kernel.

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Per-tool line-count summary for file-mutating tools.
///
/// Populated by tools that have direct access to pre- and post-mutation
/// file content at execution time (`fs_write`, `fs_edit`, `fs_delete`).
/// Surfaces upward through the kernel boundary via [`ToolResult::line_diff`]
/// and `ToolOutput::line_diff`, eventually landing on the per-task
/// `files_changed` summary aura-os-server persists for the dashboard's
/// "Lines" stat.
///
/// Tools that can't compute a diff (or for which it doesn't apply) leave
/// the field at `None`. Downstream consumers must treat `None` as
/// "unknown", not "zero" — the absence-vs-presence distinction is what
/// lets the dashboard tell "no edit happened" apart from "tool didn't
/// report counts".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineDiff {
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Classification for a tool result.
///
/// `CompactionStructural` is reserved for history-redaction placeholders
/// that were replayed as fresh tool inputs. They are surfaced to the model
/// as errors, but they are not agent mistakes for termination accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    /// Successful tool execution.
    #[default]
    Ok,
    /// Ordinary tool failure caused by invalid args, IO, policy, command
    /// failure, or any other agent/tool error.
    AgentError,
    /// Structural rejection caused by compacted/redacted history markers.
    CompactionStructural,
}

/// Result from a tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    /// Tool name
    pub tool: String,
    /// Whether the tool succeeded
    pub ok: bool,
    /// Machine-readable result classification.
    pub kind: ToolResultKind,
    /// Exit code (for commands)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Standard output
    #[serde(default, with = "crate::serde_helpers::bytes_serde")]
    pub stdout: Bytes,
    /// Standard error
    #[serde(default, with = "crate::serde_helpers::bytes_serde")]
    pub stderr: Bytes,
    /// Additional metadata
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    /// Optional per-file line diff produced by file-mutating tools
    /// (`fs_write`, `fs_edit`, `fs_delete`). `None` means "the tool
    /// didn't report counts" — consumers must not interpret it as a
    /// zero-line change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_diff: Option<LineDiff>,
}

impl<'de> Deserialize<'de> for ToolResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ToolResultWire {
            tool: String,
            ok: bool,
            #[serde(default)]
            kind: Option<ToolResultKind>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            exit_code: Option<i32>,
            #[serde(default, with = "crate::serde_helpers::bytes_serde")]
            stdout: Bytes,
            #[serde(default, with = "crate::serde_helpers::bytes_serde")]
            stderr: Bytes,
            #[serde(default, skip_serializing_if = "HashMap::is_empty")]
            metadata: HashMap<String, String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            line_diff: Option<LineDiff>,
        }

        let wire = ToolResultWire::deserialize(deserializer)?;
        let kind = wire.kind.unwrap_or(if wire.ok {
            ToolResultKind::Ok
        } else {
            ToolResultKind::AgentError
        });
        Ok(Self {
            tool: wire.tool,
            ok: wire.ok,
            kind,
            exit_code: wire.exit_code,
            stdout: wire.stdout,
            stderr: wire.stderr,
            metadata: wire.metadata,
            line_diff: wire.line_diff,
        })
    }
}

impl ToolResult {
    /// Create a successful tool result.
    #[must_use]
    pub fn success(tool: impl Into<String>, stdout: impl Into<Bytes>) -> Self {
        Self {
            tool: tool.into(),
            ok: true,
            kind: ToolResultKind::Ok,
            exit_code: None,
            stdout: stdout.into(),
            stderr: Bytes::new(),
            metadata: HashMap::new(),
            line_diff: None,
        }
    }

    /// Create a failed tool result.
    #[must_use]
    pub fn failure(tool: impl Into<String>, stderr: impl Into<Bytes>) -> Self {
        Self {
            tool: tool.into(),
            ok: false,
            kind: ToolResultKind::AgentError,
            exit_code: None,
            stdout: Bytes::new(),
            stderr: stderr.into(),
            metadata: HashMap::new(),
            line_diff: None,
        }
    }

    /// Create a structural compaction/redaction failure result.
    #[must_use]
    pub fn compaction_structural_failure(
        tool: impl Into<String>,
        stderr: impl Into<Bytes>,
    ) -> Self {
        Self {
            tool: tool.into(),
            ok: false,
            kind: ToolResultKind::CompactionStructural,
            exit_code: None,
            stdout: Bytes::new(),
            stderr: stderr.into(),
            metadata: HashMap::new(),
            line_diff: None,
        }
    }

    /// Add metadata.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Attach a typed per-file line diff. Used by `fs_write`, `fs_edit`,
    /// and `fs_delete` to surface the line counts they compute at
    /// execution time. The kernel boundary copies the value through to
    /// `ToolOutput::line_diff` so the agent loop can build accurate
    /// `FileChange` entries without re-reading the filesystem.
    #[must_use]
    pub fn with_line_diff(mut self, lines_added: u32, lines_removed: u32) -> Self {
        self.line_diff = Some(LineDiff {
            lines_added,
            lines_removed,
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolResult, ToolResultKind};

    #[test]
    fn missing_kind_defaults_from_ok_for_old_records() {
        let success: ToolResult = serde_json::from_str(
            r#"{"tool":"read_file","ok":true,"stdout":"b2s=","stderr":"","metadata":{}}"#,
        )
        .unwrap();
        assert_eq!(success.kind, ToolResultKind::Ok);

        let failure: ToolResult = serde_json::from_str(
            r#"{"tool":"write_file","ok":false,"stdout":"","stderr":"YmFk","metadata":{}}"#,
        )
        .unwrap();
        assert_eq!(failure.kind, ToolResultKind::AgentError);
    }
}
