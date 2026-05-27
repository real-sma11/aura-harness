//! Stateful steering evaluators owned by the agent loop.
//!
//! Phase 2 of the core-loop architecture refactor split the steering
//! subsystem in two:
//!
//! - `aura-prompts/src/steering/` owns the **render** half — the
//!   `SteeringKind` enum, per-variant body text, and the
//!   `<harness_steering>` envelope wrapper. That crate has no
//!   reasoner dep and no agent-loop state on its surface.
//! - This module (the **evaluation + injection** half) owns the
//!   stateful evaluators that translate live agent-loop signals into
//!   `SteeringKind` values, plus the `inject` helper that appends
//!   the rendered envelope to `Vec<aura_reasoner::Message>`.
//!
//! Phase 5 of the refactor will introduce a proper `TurnSteering`
//! trait + registry on this surface; today the registry placeholder
//! is intentionally absent — every relocation is behaviour-preserving.
//!
//! The relocation also fixes the previous layer violation
//! (`prompts/steering/implement_now_gate.rs` reaching into
//! `agent_loop::{LoopState, AgentLoopConfig}` from a sibling
//! "prompts" module): every evaluator now lives in the same crate /
//! module tree as the state it inspects.

pub mod early_oracle;
pub mod implement_now;
pub mod inject;
pub mod repeated_read;

// Re-exported so Phase 5 (and current tests) can refer to it via
// `crate::agent_loop::steering::EarlyTestOracle`. The wiring into
// `LoopState` happens in Phase 5; the underlying type is currently
// `#[allow(dead_code)]` and exercised by unit tests only.
#[allow(unused_imports)]
pub(crate) use early_oracle::EarlyTestOracle;
// `evaluate_implement_now` is `pub(crate)` because it takes a
// `LoopState` (also `pub(crate)`); the re-export must match. Phase 5
// will move this behind a `TurnSteering` trait.
pub(crate) use implement_now::evaluate_implement_now;
pub use inject::inject;
pub use repeated_read::RepeatedReadTracker;

// Phase-1 back-compat aliases (`REPEATED_READ_THRESHOLD`,
// `IMPLEMENT_NOW_DEFAULT_THRESHOLD`) are intentionally NOT re-exported
// here. Phase 2 hard-deletes them; consumers read the values directly
// from `aura_config::*`.
