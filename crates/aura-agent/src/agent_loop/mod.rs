//! Main agent loop orchestrator.
//!
//! `AgentLoop` drives the multi-step agentic conversation by calling
//! the model provider in a loop with intelligence: blocking detection,
//! compaction, sanitization, budget management, etc.
//!
//! # Module layout (Phase 3 split)
//!
//! The former monolithic `mod.rs` (~1.5K lines) has been carved into
//! purpose-driven submodules:
//!
//! - [`config`] — [`AgentLoopConfig`] + [`AgentLoopConfig::for_agent`]
//!   plus the `prompt_cache_retention` wire-string parser.
//! - [`state`] — [`LoopState`], [`state::ThinkingBudget`], and the
//!   per-iteration `begin_iteration` / `build_request` /
//!   `compute_thinking_effort` helpers.
//! - [`cache`] — [`cache::ToolResultCache`], [`cache::ReadRangeEntry`].
//! - [`run`] — [`AgentLoop::new`] / `run` / `run_with_events` /
//!   `run_with_session` / `run_inner` orchestration + the
//!   [`run::is_cancelled`] probe.
//! - [`stop_reason`] — [`AgentLoop::dispatch_stop_reason`],
//!   `apply_summary_compaction`, `build_summary_request`, and the
//!   `retry_after_context_overflow` ladder.
//!
//! Sibling modules (`iteration`, `sampling`, `tool_pipeline`,
//! `tool_execution`, `stream_pump`, `streaming`, `context`, `turn`,
//! `task`, `turn_diff`, `search_cache`, `steering`, `compaction_summary`)
//! are unchanged in shape and still call into the split submodules
//! through `super::*` re-exports listed below. Visibility of
//! [`LoopState`] / [`cache::ToolResultCache`] / etc. remains
//! `pub(crate)` per the Phase 0 demotion.

mod cache;
mod compaction_summary;
mod config;
mod context;
mod iteration;
mod run;
mod sampling;
mod search_cache;
mod state;
pub mod steering;
mod stop_reason;
mod stream_pump;
mod streaming;
mod task;
mod tool_execution;
#[cfg(test)]
mod tool_execution_tests;
mod tool_pipeline;
mod turn;
mod turn_diff;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod cutover_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod shamir_replay_tests;
#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_advanced;

pub use config::AgentLoopConfig;
pub use task::TaskId;

pub(crate) use cache::{ReadRangeEntry, ToolResultCache};
pub(crate) use run::is_cancelled;
pub(crate) use state::LoopState;

/// The main multi-step agent loop orchestrator.
///
/// Public constructors / entry points live in [`run`]; the per-run
/// mutable state and request-building lives in [`state`]; the
/// stop-reason dispatch tail lives in [`stop_reason`]. See the
/// module-level docs above for the full split.
pub struct AgentLoop {
    pub(super) config: AgentLoopConfig,
}
