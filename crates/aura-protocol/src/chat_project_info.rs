//! Wire-compatible chat project descriptor.
//!
//! Typed project metadata threaded onto every
//! [`crate::RuntimeRequest`] so the harness's `SystemPromptBuilder`
//! can render `<project_context>` from structured fields rather than
//! receiving a server-baked prompt string. Mirrors the pattern used
//! for the persona fields in [`crate::AgentPersona`].
//!
//! Lives in `aura-protocol` (rather than `aura-os-harness`) so any
//! future client-side caller (eval harness, OpenAPI generator, etc.)
//! can reach it from a stable location.

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

/// Typed project descriptor surfaced into the chat-path system prompt's
/// `<project_context>` section.
///
/// Every field is best-effort prose: aura-os reads it off the
/// `projects` row, the harness splices it into the prompt verbatim.
/// Empty / blank fields are dropped at render time so an
/// uninitialised wire payload produces no stray `description: ` /
/// `build_command: ` lines.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ChatProjectInfoWire {
    /// Stable project UUID, surfaced as `project_id` in the
    /// `<project_context>` block.
    #[serde(default)]
    pub id: String,
    /// Operator-authored project name.
    #[serde(default)]
    pub name: String,
    /// Operator-authored project description. Blank ⇒ the renderer
    /// drops the `description:` line entirely.
    #[serde(default)]
    pub description: String,
    /// Absolute workspace root on the harness host. Surfaced as the
    /// `folder:` line of `<project_context>` and used by
    /// `agents_md_from_workspace()` to locate `AGENTS.md`.
    #[serde(default)]
    pub workspace_root: String,
    /// Configured build command (e.g. `cargo build`). Blank ⇒ the
    /// renderer drops `build_command:`.
    #[serde(default)]
    pub build_command: String,
    /// Configured test command (e.g. `cargo test`). Blank ⇒ the
    /// renderer drops `test_command:`.
    #[serde(default)]
    pub test_command: String,
}

impl ChatProjectInfoWire {
    /// True when every field is blank — i.e. the wire payload carries
    /// no usable project descriptor. The harness uses this to decide
    /// whether the typed-fields path applies at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id.trim().is_empty()
            && self.name.trim().is_empty()
            && self.description.trim().is_empty()
            && self.workspace_root.trim().is_empty()
            && self.build_command.trim().is_empty()
            && self.test_command.trim().is_empty()
    }
}
