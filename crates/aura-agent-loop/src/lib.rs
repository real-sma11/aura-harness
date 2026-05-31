//! # aura-agent-loop
//!
//! Layer: agent
//!
//! Phase 6a thin re-export shell over [`aura_agent`]'s turn loop. The
//! agent loop currently lives in `aura-agent` (the legacy single-crate
//! agent layer); Phase 6a establishes `aura-agent-loop` as the
//! workspace-visible name future consumers (notably `aura-fleet-spawn`
//! in Phase 7a) bind to so the upcoming full extraction is a flat
//! rename inside this crate rather than a cross-workspace rewire.
//!
//! ## Public surface
//!
//! - [`AgentLoop`] — the multi-step agent loop orchestrator.
//! - [`AgentLoopConfig`] — its configuration struct.
//! - [`RunOptions`] — per-invocation knobs.
//! - [`TurnEvent`], [`TurnEventSink`] — observable turn-level events.
//! - [`AgentLoopResult`], [`AgentLoopError`] — turn outcome types
//!   (the error alias is `aura_agent::AgentError`).
//!
//! See [`aura_agent`] for the concrete implementations; this crate
//! intentionally adds no logic on top.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_agent::{
    AgentError as AgentLoopError, AgentLoop, AgentLoopConfig, AgentLoopEvent, AgentLoopResult,
    RunOptions, TurnEvent, TurnEventSink,
};
