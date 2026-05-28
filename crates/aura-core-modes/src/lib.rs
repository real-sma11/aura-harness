//! # aura-core-modes
//!
//! Layer: core
//!
//! Closed-enum mode primitives. Every external effect dispatches off
//! [`AgentMode`] before consulting user permissions.
//!
//! ## Closed-enum invariant
//!
//! Adding a variant to [`AgentMode`], [`KernelMode`], [`SpawnMode`],
//! [`JoinPolicy`], [`ReplayMode`], or [`SandboxMode`] is a breaking
//! change. Plugins and downstream consumers cannot extend these enums.
//! All match arms in this crate must be exhaustive (no `_` wildcard)
//! so the compiler catches missing cases when a new variant is
//! introduced.
//!
//! ## Mode-narrowing invariant
//!
//! [`AgentMode::default_capability_profile`] is the per-mode ceiling.
//! Effective permissions are always `mode ∩ user_grants` — mode can
//! only NARROW user grants, never widen them. The intersection is
//! computed by `aura-core-permissions::effective`.
//!
//! ## Action gate
//!
//! [`ModeGate::check`] returns [`ModeViolation`] for every effect that
//! is disallowed in the current mode. The gate fires BEFORE any
//! permission check so the mode is always the outer wrapper.
//!
//! ## Failure modes
//!
//! - [`ModeViolation`] — one variant per blocked action class. Wire
//!   format is stable (`#[serde(tag = "kind", rename_all = "snake_case")]`).
//! - Replay mismatch: detected by higher-layer kernel; this crate has
//!   no I/O and cannot itself fail.
//!
//! ## Assumptions
//!
//! - Mode is resolved once per session by the surface binary and
//!   threaded explicitly through every call site. Library crates
//!   never re-resolve mode.
//! - `CapabilityProfile` lists capability discriminants as
//!   `&'static str` to keep this crate a leaf (zero `aura-*` deps).
//!   The permissions crate maps these back to its richer `Capability`
//!   enum.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod capability_profile;
mod gate;
mod modes;
mod profile;
mod violation;

pub use capability_profile::CapabilityProfile;
pub use gate::{DefaultModeGate, GatedAction, ModeGate};
pub use modes::{AgentMode, AgentRole, JoinPolicy, KernelMode, ReplayMode, SandboxMode, SpawnMode};
pub use profile::ModeProfile;
pub use violation::ModeViolation;
