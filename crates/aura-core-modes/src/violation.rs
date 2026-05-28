//! [`ModeViolation`] — one variant per blocked action class.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Closed error enum returned by [`crate::ModeGate::check`] when an
/// action is disallowed by the current [`crate::AgentMode`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModeViolation {
    /// Subagent spawning is disallowed in the current mode.
    #[error("subagent spawning is not allowed in this mode")]
    SpawnNotAllowed,
    /// Non-markdown write is disallowed (Plan/Ask/Debug).
    #[error("writing non-markdown files is not allowed in this mode")]
    WriteNonMarkdownNotAllowed,
    /// All file writes are disallowed (Ask/Debug).
    #[error("file writes are not allowed in this mode")]
    WriteNotAllowed,
    /// Subprocess execution is disallowed in the current mode.
    #[error("subprocess execution is not allowed in this mode")]
    SubprocessNotAllowed,
    /// Network access is disallowed in the current mode.
    #[error("network access is not allowed in this mode")]
    NetworkNotAllowed,
    /// Plugin activation is disallowed in the current mode.
    #[error("plugin activation is not allowed in this mode")]
    PluginActivationNotAllowed,
    /// Tool invocation is disallowed for the named tool.
    #[error("tool invocation '{tool}' is not allowed in this mode")]
    ToolInvocationNotAllowed {
        /// Name of the rejected tool.
        tool: String,
    },
    /// A non-sandboxed probe was requested in Debug mode.
    #[error("only sandboxed probes are allowed in Debug mode")]
    NonSandboxedActionInDebug,
}
