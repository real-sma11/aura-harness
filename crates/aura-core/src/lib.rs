//! # aura-core
//!
//! Layer: core (compatibility shell)
//!
//! Phase 1 compatibility shell. Permission, mode, and selected new
//! id primitives have moved to dedicated layered crates
//! (`aura-core-permissions`, `aura-core-modes`, `aura-core-types`)
//! and are re-exported here verbatim so existing call sites keep
//! compiling.
//!
//! All domain types now live in `aura-core-types`; this crate is a
//! pure re-export shell over it.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_core_types::*;
pub use aura_core_types::hash;
