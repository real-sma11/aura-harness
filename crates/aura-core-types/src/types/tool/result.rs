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

/// A single rendered image attached to a [`ToolResult`].
///
/// Computer-use / vision tools (e.g. the `computer` tool) return a
/// screenshot alongside their textual result. The bytes are carried as
/// a base64 string (NOT raw [`Bytes`]) so the value round-trips through
/// the kernel effect log and the outbound wire boundary as plain JSON
/// without a second encode/decode hop.
///
/// Invariant: `base64` is the base64-encoded payload of an image whose
/// IANA media type is `media_type` (e.g. `"image/png"`). Never log
/// `base64`; log `media_type` and the encoded length instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultImage {
    /// Base64-encoded image payload (PNG/JPEG).
    pub base64: String,
    /// IANA media type of [`Self::base64`] (e.g. `"image/png"`).
    pub media_type: String,
}

impl ToolResultImage {
    /// Construct an image attachment from its base64 payload + media type.
    #[must_use]
    pub fn new(base64: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self {
            base64: base64.into(),
            media_type: media_type.into(),
        }
    }
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
    /// Optional rendered image produced by computer-use / vision tools
    /// (e.g. the `computer` tool's screenshot). `None` for every
    /// text-only tool. Strictly additive: older records omit the field
    /// and it deserializes to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ToolResultImage>,
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
            #[serde(default, skip_serializing_if = "Option::is_none")]
            image: Option<ToolResultImage>,
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
            image: wire.image,
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
            image: None,
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
            image: None,
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
            image: None,
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

    /// Attach a rendered image (base64 + media type) to this result.
    ///
    /// Used by computer-use / vision tools (e.g. the `computer` tool)
    /// to carry a screenshot alongside the textual result so the kernel
    /// boundary and the outbound wire can replay it to the model as an
    /// image block.
    #[must_use]
    pub fn with_image(mut self, base64: impl Into<String>, media_type: impl Into<String>) -> Self {
        self.image = Some(ToolResultImage::new(base64, media_type));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolResult, ToolResultImage, ToolResultKind};

    #[test]
    fn tool_result_without_image_omits_field_and_round_trips() {
        // The text-only path must serialize byte-identically to the
        // pre-image contract: no `image` key appears when `None`.
        let result = ToolResult::success("read_file", b"hello".to_vec());
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(
            !json.contains("image"),
            "absent image must be skipped: {json}"
        );
        let back: ToolResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, result);
        assert!(back.image.is_none());
    }

    #[test]
    fn tool_result_image_round_trips() {
        // A computer-use screenshot result must round-trip the image
        // attachment. The base64 here is a tiny placeholder.
        let result = ToolResult::success("computer", b"screenshot taken".to_vec())
            .with_image("aGVsbG8=", "image/png");
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"base64\":\"aGVsbG8=\""), "{json}");
        assert!(json.contains("\"media_type\":\"image/png\""), "{json}");

        let back: ToolResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, result);
        assert_eq!(
            back.image,
            Some(ToolResultImage::new("aGVsbG8=", "image/png"))
        );
    }

    #[test]
    fn tool_result_legacy_json_deserializes_without_image() {
        let legacy: ToolResult = serde_json::from_str(
            r#"{"tool":"read_file","ok":true,"stdout":"aGk=","stderr":"","metadata":{}}"#,
        )
        .expect("legacy payload deserializes");
        assert!(legacy.image.is_none());
    }

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
