//! # aura-exec-runner
//!
//! Layer: exec
//!
//! Phase 5 layered alias for the `ToolExecutor` (the orchestrator
//! that dispatches `ToolCall`s through the `Tool` trait).
//!
//! ## Why this crate is a re-export shell
//!
//! Same rationale as [`aura_exec_tools`]: the runtime executor is
//! tightly coupled to the entire `aura-tools` module graph
//! (`crate::error`, `crate::sandbox`, `crate::tool`,
//! `crate::agents::cross_agent_catalog_entries`,
//! `crate::ToolConfig`). A literal file-move into a new crate would
//! require rewriting every internal use-path for no behavioural gain.
//! Per the plan's "Documented partial-deviation > red build" rule
//! the runner currently re-exports the legacy `ToolExecutor` under
//! the layered name and exposes the new conflict + isolation
//! primitives that Phase 6+ work will wire into the executor.
//!
//! ## What this crate adds
//!
//! - `pub use aura_exec_tools::*` — the legacy tool + executor
//!   surface.
//! - [`conflict`] — re-exports [`aura_exec_conflict::ConflictRegistry`]
//!   etc. so the orchestration layer can grab advisory locks before
//!   dispatching.
//! - [`isolation`] — re-exports [`aura_exec_isolation::Isolation`]
//!   etc. so subagent runners can provision a private workspace.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_exec_tools::*;

/// Phase 5 advisory-lock primitives surfaced under the exec-runner
/// namespace. The runner will consume
/// `aura_config::ConflictConfig::default_wait_ms` as the default
/// budget when it grows a real acquire path in Phase 6.
pub mod conflict {
    pub use aura_exec_conflict::{ConflictDomain, ConflictError, ConflictRegistry, LockHandle};
}

/// Phase 5 subagent isolation primitives surfaced under the
/// exec-runner namespace.
pub mod isolation {
    pub use aura_exec_isolation::{
        CopyIsolation, IsolatedWorkspace, Isolation, IsolationError, IsolationStrategy,
        WorktreeIsolation,
    };
}
