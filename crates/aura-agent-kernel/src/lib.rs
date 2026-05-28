//! # aura-kernel
//!
//! Deterministic kernel for Aura.
//!
//! This crate provides:
//! - Single-step kernel processing (Spec-01 legacy)
//! - Policy engine for authorization
//! - Context building for model requests
//!
//! ## Architecture
//!
//! The kernel is the deterministic core of AURA. It:
//! 1. Builds context from the record window
//! 2. Calls the model provider for completions
//! 3. Applies policy to authorize actions
//! 4. Executes actions via the executor router
//! 5. Records all inputs/outputs for replay
//!
//! The turn processor and process manager now live under `aura-agent::runtime`.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

// TODO(Phase 8): GitExecutor for kernel-recorded git mutations
// TODO(Phase 8): BuildVerifyExecutor for kernel-recorded build/test

pub(crate) mod billing;
mod context;
pub(crate) mod executor;
mod kernel;
mod policy;
pub(crate) mod router;
pub(crate) mod spawn_hook;

pub use billing::walk_parent_chain;
pub use context::{hash_tx_with_window, Context, ContextBuilder};
pub use executor::{
    decode_tool_effect, DecodedToolResult, ExecuteContext, ExecuteLimits, Executor, ExecutorError,
};
pub use kernel::{
    write_system_record, ApprovalRequiredInfo, Kernel, KernelConfig, PendingToolPrompt,
    ProcessResult, ReasonResult, ReasonStreamHandle, ToolApprovalError, ToolApprovalPrompter,
    ToolApprovalRemember, ToolApprovalResponse, ToolDecision, ToolOutput,
};
pub use policy::{Policy, PolicyConfig, PolicyResult, PolicyVerdict};
pub use router::ExecutorRouter;
pub use spawn_hook::{
    ChildAgentSpec, KernelSpawnHook, NoopSpawnHook, SpawnError, SpawnHook, SpawnOutcome,
};

/// Errors from the deterministic kernel (store, reasoner, serialization).
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("store error: {0}")]
    Store(String),
    /// Provider-side reasoning failure. The inner
    /// [`aura_reasoner::ReasonerError`] is preserved (instead of being
    /// flattened to a string) so callers — notably
    /// `aura_agent::KernelModelGateway` — can branch on the variant
    /// (`RateLimited`, `InsufficientCredits`, `Api { status }`, …)
    /// rather than string-matching the formatted message. The kernel's
    /// own audit-log writers stringify this via
    /// [`std::error::Error::source`] / [`std::fmt::Display`] before
    /// recording, so the `Reasoning` record payload format is
    /// unchanged.
    #[error("reasoner error: {0}")]
    Reasoner(#[from] aura_reasoner::ReasonerError),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("{0}")]
    Internal(String),
}

// Re-export ToolResultContent for convenience
pub use aura_core::ToolResultContent;
