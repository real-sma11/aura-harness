//! # aura-agent
//!
//! Multi-step agentic orchestration layer for AURA.
//!
//! This crate owns the intelligent agent loop that wraps the kernel's
//! single-step processing. It provides:
//!
//! - `AgentLoop` — the main multi-step orchestrator
//! - Blocking detection — prevents infinite loops on failing tools
//! - Read guards — limits redundant file re-reads
//! - Context compaction — tiered message truncation to stay within token limits
//! - Message sanitization — repairs message history for API validity
//! - Budget tracking — exploration, token, and credit budget management
//! - Build integration — auto-build checks after write operations
//!
//! ## Architecture
//!
//! `aura-agent` sits between the presentation layer (CLI, terminal, swarm)
//! and the kernel. It calls the step processor in a loop, adding intelligence
//! at each iteration.
//!
//! ```text
//! Presentation → AgentLoop → StepProcessor → ModelProvider + Tools
//! ```

#![forbid(unsafe_code)]
#![allow(clippy::module_name_repetitions)]
// Line/column numbers and small counters never exceed i32::MAX or lose f64 precision
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]
// Internal crate — error docs for pub(crate) functions add noise
#![allow(clippy::missing_errors_doc)]
// Prompt-building code uses push_str(&format!()) extensively for clarity
#![allow(clippy::format_push_string)]
// Many match-to-let-else refactors would reduce readability in complex control flow
#![allow(clippy::manual_let_else)]
// Mutex guard drop timing is correct; tightening adds complexity for marginal benefit
#![allow(clippy::significant_drop_tightening)]
// Result wrappers kept for forward-compatibility (functions may gain error paths)
#![allow(clippy::unnecessary_wraps)]
// if-let-else is often more readable than map_or/map_or_else closures
#![allow(clippy::option_if_let_else)]

mod agent_loop;
mod budget;
pub(crate) mod build;
pub mod constants;
pub(crate) mod events;
pub(crate) mod file_ops;
pub mod git;
pub mod helpers;
mod kernel_domain_gateway;
mod kernel_gateway;
pub(crate) mod planning;
pub mod prompts;
mod recording_stream;
mod sanitize;
pub(crate) mod self_review;
pub(crate) mod turn_config;
pub mod types;
pub(crate) mod verify;

pub mod agent_runner;
pub mod runtime;
pub mod session_bootstrap;
pub(crate) mod task_context;
pub(crate) mod task_executor;

pub use agent_loop::{AgentLoop, AgentLoopConfig};
pub use constants::{tool_result_cache_key, CACHEABLE_TOOLS, DEFAULT_MODEL, FALLBACK_MODEL};
pub use events::{map_agent_loop_event, AgentLoopEvent, DebugEvent, TurnEvent, TurnEventSink};
pub use kernel_domain_gateway::{KernelDomainGateway, KernelDomainGatewayError};
pub use kernel_gateway::{KernelModelGateway, KernelToolGateway, RecordingModelProvider};
pub use runtime::{
    ProcessManager, ProcessManagerConfig, ProcessOutput, RunningProcess, RuntimeError,
};
pub use types::{
    AgentLoopResult, AgentToolExecutor, AutoBuildResult, BuildBaseline, FileChange, FileChangeKind,
    ToolCallInfo, ToolCallResult, TurnObserver, TurnObservers,
};

/// Errors arising from the agent orchestration loop (model calls, tool execution, timeouts).
///
/// Phase 5 (error-handling polish) reshaped this enum so that
/// [`aura_reasoner::ReasonerError`] and [`aura_kernel::KernelError`]
/// are preserved end-to-end instead of being flattened to a `String`
/// inside [`AgentError::Internal`]. Callers (and tests) can now match
/// on the underlying variant — for example
/// `AgentError::Reason(ReasonerError::RateLimited { .. })` — to
/// implement variant-specific retry / billing / surfacing behaviour
/// without parsing the formatted message.
///
/// `Display` for [`Reason`] and [`Kernel`] is `transparent`: the
/// rendered text is exactly the inner error's `Display`, so log output
/// never double-wraps as `"Internal: Reason: ..."`.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Provider-side failure surfaced through the kernel gateway. The
    /// inner [`aura_reasoner::ReasonerError`] keeps its variant
    /// (`RateLimited`, `InsufficientCredits`, `Api { status }`,
    /// `StreamAbortedWithPartial`, …) so the agent loop and CLI can
    /// branch on it.
    #[error(transparent)]
    Reason(#[from] aura_reasoner::ReasonerError),
    /// Non-reasoner kernel failure (store, serialization, internal
    /// invariants, kernel-level timeout). Reasoner failures wrapped
    /// in [`aura_kernel::KernelError::Reasoner`] are unwrapped into
    /// [`AgentError::Reason`] via [`From<KernelError>`] below.
    #[error(transparent)]
    Kernel(aura_kernel::KernelError),
    /// `tokio::spawn_blocking` join failure — the worker panicked or
    /// was cancelled by the runtime.
    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("model error: {0}")]
    Model(String),
    #[error("tool execution error: {0}")]
    ToolExecution(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("build failed: {0}")]
    BuildFailed(String),
    #[error("{0}")]
    Internal(String),
}

impl From<aura_kernel::KernelError> for AgentError {
    /// Flatten `KernelError::Reasoner(inner)` into
    /// [`AgentError::Reason`] so the typed `ReasonerError` survives a
    /// trip through the kernel gateway and a `?` in the agent runner
    /// without being wrapped twice (which would render as
    /// `"reasoner error: <ReasonerError display>"`). All other kernel
    /// failure modes flow through [`AgentError::Kernel`] unchanged.
    fn from(e: aura_kernel::KernelError) -> Self {
        match e {
            aura_kernel::KernelError::Reasoner(inner) => Self::Reason(inner),
            other => Self::Kernel(other),
        }
    }
}

#[cfg(test)]
mod event_sequence_tests;
#[cfg(test)]
mod store_migration_tests;
