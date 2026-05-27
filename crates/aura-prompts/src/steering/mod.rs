//! Per-iteration model-facing steering envelopes.
//!
//! Phase 2 split:
//!
//! - **Rendering** (this crate): [`SteeringKind`] enum +
//!   [`SteeringRenderer`]. Stateless. Returns a `String`. No
//!   `Vec<aura_reasoner::Message>` mutation, no `LoopState`, no
//!   evaluator state machines.
//! - **Evaluation + appending** (`aura-agent/src/agent_loop/steering/`):
//!   the `repeated_read`, `implement_now`, and `early_oracle`
//!   evaluators that decide *when* to fire a steering kind, plus the
//!   `inject` helper that appends the rendered envelope to
//!   `state.messages` via `aura-agent::helpers::append_warning`.
//!
//! Every per-iteration injection the harness emits flows through
//! [`SteeringRenderer`] and ends up wrapped in
//! `<harness_steering kind="...">...</harness_steering>` in the
//! transcript. The kind label lets the model pattern-match the
//! envelope when deciding how to react.

mod kind;
mod messages;

#[cfg(test)]
mod tests;

pub use kind::{SteeringKind, SteeringRenderer, StubReportView};
