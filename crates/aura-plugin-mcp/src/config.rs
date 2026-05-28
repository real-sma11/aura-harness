//! Server configuration mirrored from a manifest `[[contributes.mcp]]`
//! entry.
//!
//! The shape lives in this crate (rather than re-using
//! `aura_plugin_core::McpContribution` directly) so the runtime
//! manager doesn't need to pull `aura-plugin-core` as a dependency.
//! The bridge from the manifest entry to a [`ServerConfig`] is the
//! caller's responsibility — Phase 8 will land that bridge inside
//! the runtime contribution loader.

use std::collections::BTreeMap;

/// MCP server contribution as the manager understands it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Stable MCP server identifier. The first-active-wins merge key
    /// (see [`crate::McpConnectionManager`]).
    pub server_id: String,
    /// Command binary to spawn (resolved verbatim by the OS — see
    /// `aura-plugin-hooks` for the path-resolution policy if the
    /// MCP transport ever borrows it; today the manager passes the
    /// string straight to [`std::process::Command::new`]).
    pub command: String,
    /// Command-line arguments. Default empty.
    pub args: Vec<String>,
    /// Env variables passed to the spawned server. The manager does
    /// NOT inherit the parent process env — it clears the child env
    /// and populates it only from this map. Operator secrets must
    /// not leak into a third-party MCP server.
    pub env: BTreeMap<String, String>,
}
