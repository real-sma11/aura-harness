//! # aura-agent-subagent
//!
//! Layer: agent
//!
//! Subagent derivation + inheritance.
//!
//! Every subagent is a derivative of a parent agent (or parent flow) —
//! the rules that decide which session state the child inherits, which
//! overrides are allowed, and which combinations are rejected outright
//! live exclusively in this crate. The fleet layer
//! (`aura-fleet-spawn`, Phase 7a) consumes the [`SubagentSpec`]
//! produced here; it never constructs derivation state itself.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - **Narrowing-only**: derived permissions / mode / model / tool
//!   sets are ALWAYS strict subsets of the parent. Any attempt to
//!   widen produces a typed [`DerivationError`] before fleet-layer
//!   resource acquisition starts.
//! - **`UserId` non-overridable**: children always run as the same
//!   user; the security boundary forbids re-targeting.
//! - **Audit attribution non-overridable**: parent agent id /
//!   parent turn id / parent tool-call id are stamped into the
//!   derived spec by this crate, not by the caller.
//! - **Mode narrowing table**:
//!     - `Agent` may spawn `Agent | Plan | Ask | Debug`.
//!     - `Plan` may only spawn `Plan | Ask | Debug` — and only if
//!       [`AgentMode::allows_spawn`] is `true` (today: only `Agent`).
//!     - `Ask` and `Debug` may spawn only same-mode children — and
//!       only if [`AgentMode::allows_spawn`] is `true`.
//!
//!   The combination of "must narrow" and "must allow_spawn" yields
//!   today's hard rule: only `Agent` mode can spawn.
//! - **Default inheritance**: every field not present in
//!   [`SubagentOverrides`] inherits from the parent — children
//!   default to the same mode/permissions/model the parent runs
//!   under, with [`KernelMode::AuditedLite`] as the per-child kernel
//!   default (children are summary-recorded).
//!
//! ## Assumptions
//!
//! - [`ParentContext`] is a stable snapshot of the parent's session
//!   state at spawn time — captured atomically by the caller before
//!   any concurrent parent-state mutation can race the derivation.
//! - Depth is validated BEFORE any fleet-layer work begins, so a
//!   rejected spawn never holds quota or isolation resources.
//!
//! ## Failure modes
//!
//! - [`DerivationError`] — closed taxonomy of every rejection reason.
//!   Every variant is tested.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod derivation;
mod errors;
mod manifest;
mod overrides;
mod parent;
mod spec;

pub use derivation::{
    DefaultDerivation, FlowDerivation, SubagentDerivation, SubagentDerivationConfig,
};
pub use errors::DerivationError;
pub use manifest::{OverriddenField, OverrideManifest};
pub use overrides::{SubagentBudget, SubagentOverrides};
pub use parent::ParentContext;
pub use spec::{AuditAttribution, SubagentLineage, SubagentSpec};

#[cfg(test)]
mod tests;
