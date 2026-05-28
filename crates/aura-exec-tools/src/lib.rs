//! # aura-exec-tools
//!
//! Layer: exec
//!
//! Phase 5 layered alias for the `Tool` trait, the built-in tool
//! catalog, the per-tool config, and the `git_*` allowlist constants.
//!
//! ## Why this crate is a re-export shell
//!
//! The plan §3 dep graph has `aura-exec-tools` as the home for the
//! `Tool` trait + every builtin impl. The pre-Phase-5 `aura-tools`
//! crate already owns that surface (50+ source files, ~10k lines of
//! tightly coupled `crate::*` imports across `fs_tools/`,
//! `git_tool/`, `agents/`, `domain_tools/`, `http_tool.rs`,
//! `automaton_tools.rs`, `catalog.rs`, `definitions.rs`, etc.). A
//! literal `git mv` of every module into this crate would have to
//! rewrite hundreds of internal use-paths and risk a red build for a
//! refactor that delivers no end-user behaviour. Per the plan's
//! "Documented partial-deviation > red build" guidance the Phase 5
//! end state is therefore:
//!
//! - `aura-tools` keeps the implementations.
//! - `aura-exec-tools` is a thin re-export under the layered name so
//!   the dep graph (`aura-exec-runner → aura-exec-tools`,
//!   `aura-exec-tools → aura-exec-sandbox` etc.) resolves.
//! - A future phase migrates the implementations into this crate's
//!   `src/` once the resolver + catalog layout has stabilised. New
//!   exec-layer features land here directly.
//!
//! ## Re-export surface
//!
//! The glob re-export of `aura_tools::*` covers every public item the
//! legacy crate already exposed (`Tool`, `ToolContext`, `ToolConfig`,
//! `ToolError`, `Sandbox`, `ToolCatalog`, `ToolResolver`,
//! `GIT_*_TOOL_NAMES`, etc.). The sandbox + policy primitives from
//! `aura-exec-sandbox` / `aura-exec-policy` are re-exported under the
//! `sandbox` / `policy` sub-namespaces so consumers can write
//! `aura_exec_tools::sandbox::FsSandbox` without pulling the
//! lower-layer crate directly.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_tools::*;

/// Phase 5 sandbox primitives surfaced under the exec-tools
/// namespace. See [`aura_exec_sandbox`] for the trait + invariants.
pub mod sandbox {
    pub use aura_exec_sandbox::{FsSandbox, ProcessSandbox, SandboxError};
}

/// Phase 5 policy verdict helper surfaced under the exec-tools
/// namespace. See [`aura_exec_policy`] for invariants.
pub mod policy {
    pub use aura_exec_policy::{evaluate, PolicyError, ToolApproval};
}
