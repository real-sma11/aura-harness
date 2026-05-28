//! # aura-surface-automaton
//!
//! Layer: surface
//!
//! Phase 9 relocation shell for the headless automaton host. The
//! underlying automaton runtime (built-in flows, scheduling,
//! TickContext, event surface) continues to live in the legacy
//! `aura-automaton` crate; this surface-layer crate re-exports it
//! so the layered `aura-<layer>-<name>` convention applies the
//! same way it does for `aura-fleet-*` and `aura-surface-*`
//! crates.
//!
//! `aura-automaton` may not migrate its internal modules in Phase 9
//! to avoid disrupting every workspace import site; new consumers
//! should prefer the `aura_surface_automaton::*` path so the
//! eventual move is a single rename.
//!
//! ## Invariants ([`.cursor/rules.md`] §13)
//!
//! - Zero runtime behaviour added. The re-export is intentionally
//!   complete (`pub use aura_automaton::*`); any breaking change
//!   to the automaton public API is a Phase 10+ task.
//! - No upward dependency on `aura-runtime` or `aura-fleet-*`. The
//!   automaton host is composed by the surface-layer entry-points
//!   (`aura-surface-cli`), never the other way around.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_automaton::*;
