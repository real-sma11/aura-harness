//! # aura-agent-steering
//!
//! Layer: agent
//!
//! Stateful steering evaluators + per-turn registry extracted from
//! `aura-agent::agent_loop::steering` during Phase 6a of the
//! agent-first architecture refactor.
//!
//! ## What this crate owns
//!
//! - The [`TurnSteering`] trait every per-turn evaluator implements.
//! - The [`SteeringRegistry`] that drives every installed source
//!   uniformly via `observe_tool` / `begin_turn` /
//!   `drain_for_next_turn`.
//! - The evaluator family: [`RepeatedReadTracker`],
//!   [`ImplementNowSteering`], [`EarlyTestOracle`].
//! - The [`inject`] helper that renders a
//!   [`aura_prompts::SteeringKind`] and appends it to a
//!   `Vec<aura_reasoner::Message>` user-message stream.
//! - The small data shapes the evaluators observe — [`ToolCallInfo`],
//!   [`ToolCallResult`], [`FileChange`], [`FileChangeKind`] — plus
//!   the tool-predicate helpers `is_exploration_tool` /
//!   `is_write_tool` and the `content_hash_hex` content-hash
//!   primitive. These shapes were previously owned by `aura-agent`
//!   directly; Phase 6a relocated them below the agent loop so the
//!   loop can depend on `aura-agent-steering` without inverting the
//!   layer order. `aura-agent` re-exports them so existing call
//!   sites (`crate::types::ToolCallInfo`, `crate::helpers::*`) keep
//!   working unchanged.
//!
//! ## Layer
//!
//! `agent` (sits BELOW the agent loop). Direct dependencies are
//! limited to `aura-core`, `aura-config`, `aura-prompts`, and
//! `aura-reasoner` — no upward edge to `aura-agent`. The advisory
//! `tests/layer_boundary.rs` check enforces this on every CI run.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod helpers;
mod registry;
mod types;

pub mod early_oracle;
pub mod implement_now;
pub mod inject;
pub mod repeated_read;

pub use helpers::{append_warning, content_hash_hex, is_exploration_tool, is_write_tool};
pub use inject::inject;
pub use registry::{SteeringRegistry, TurnSteering};
pub use types::{FileChange, FileChangeKind, ToolCallInfo, ToolCallResult};

pub use early_oracle::EarlyTestOracle;
pub use implement_now::ImplementNowSteering;
pub use repeated_read::RepeatedReadTracker;
