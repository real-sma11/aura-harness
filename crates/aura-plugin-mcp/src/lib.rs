//! # aura-plugin-mcp
//!
//! Layer: plugin
//!
//! Stdio Model-Context-Protocol client + first-active-wins connection
//! manager for first-party Aura plugins. Phase 4c deliverable: ships
//! the in-process pool wrapping a child process per server id. No
//! caller in the agent loop instantiates the manager yet (Phase 8
//! wires the per-session contribution flow).
//!
//! ## Surfaces
//!
//! - [`McpClient`] — newline-delimited JSON-RPC 2.0 client over the
//!   server's stdio pipes. Owns one child process and a monotonic
//!   request id counter.
//! - [`McpConnectionManager`] — a pool of [`McpClient`]s keyed by
//!   `server_id`. First-active-wins merge: when two plugins
//!   contribute the same `server_id`, the first one registered keeps
//!   the slot; subsequent registrations error with
//!   [`McpError::DuplicateServer`] and are warn-logged (the caller
//!   may downgrade the error to a no-op for ergonomic CLI flows).
//! - [`ServerConfig`] — config shape mirrored from the
//!   `[[contributes.mcp]]` manifest entry.
//! - Error type: [`McpError`].
//!
//! ## Invariants ([rules.md §13])
//!
//! - Each [`McpClient`] owns one child process and one
//!   `(writer, reader)` pair against that child's stdio.
//! - The request id sequence is monotone per client (1..) and the
//!   server is expected to echo it back in the response `"id"` field.
//! - On child exit or pipe error the client returns
//!   [`McpError::Disconnected`] for every subsequent call. The
//!   manager is responsible for restart (Phase 8+); Phase 4c does
//!   not auto-restart.
//! - The manager spawns child processes with an explicitly cleared
//!   env populated only from [`ServerConfig::env`]. There is no
//!   parent-env inheritance — operator secrets must not leak into a
//!   third-party MCP server.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod client;
pub mod config;
pub mod error;
pub mod manager;

pub use client::McpClient;
pub use config::ServerConfig;
pub use error::McpError;
pub use manager::McpConnectionManager;
