//! Error type for the MCP client + manager.

use thiserror::Error;

/// MCP transport / protocol errors. Surfaced from both
/// [`crate::McpClient`] (per-request) and [`crate::McpConnectionManager`]
/// (registration / lookup).
#[derive(Debug, Error)]
pub enum McpError {
    /// Low-level I/O error from the child process's stdio pipes or
    /// from the spawn itself.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Child process exited or pipe broke before / during the
    /// request. The manager owns restart policy (Phase 8+); Phase 4c
    /// surfaces this so the caller can decide.
    #[error("MCP transport disconnected")]
    Disconnected,
    /// Server response was not valid JSON or did not conform to the
    /// JSON-RPC 2.0 shape. The carried string is the underlying
    /// parse error message.
    #[error("invalid jsonrpc response: {0}")]
    InvalidResponse(String),
    /// Server returned a JSON-RPC `"error"` object.
    #[error("server returned error: code={code} message={message}")]
    ServerError {
        /// JSON-RPC error code.
        code: i32,
        /// JSON-RPC error message.
        message: String,
    },
    /// Two plugins contributed the same `server_id` — first-active-wins
    /// merge surfaces the duplicate as a warn-able error rather than
    /// silently dropping the second contribution.
    #[error("duplicate MCP server id: {0}")]
    DuplicateServer(String),
    /// `with_client` was called against an id that is not currently
    /// registered.
    #[error("unknown MCP server id: {0}")]
    UnknownServer(String),
}
