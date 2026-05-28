//! # aura-kernel (Phase 6a compatibility shell)
//!
//! Re-exports the deterministic kernel from
//! [`aura_agent_kernel`]. Phase 6a renamed the original
//! `aura-kernel` crate to `aura-agent-kernel` to fit the layered
//! `aura-<layer>-<name>` convention. This shell preserves the
//! historical `aura_kernel::*` import paths so the workspace can
//! continue to compile while consumers migrate.
//!
//! Layer: agent (shell)
#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_agent_kernel::*;
