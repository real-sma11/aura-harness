//! # aura-fleet-subagent
//!
//! Layer: fleet
//!
//! Fleet-layer concrete [`aura_tools::SubagentDispatchHook`] impl.
//!
//! Phase B / Commit 3 splits the legacy `aura-runtime/src/subagent_dispatch.rs`
//! god-file across three layers:
//!
//! - [`aura_tools::SubagentDispatchHook`] (exec) — the trait the `task`
//!   tool consumes. Unchanged.
//! - [`aura_agent_subagent`] (agent) — `SubagentRegistry`, bundled
//!   kinds, and the pure-data adapter helpers
//!   (`parent_context_from_request`, `overrides_from_request`,
//!   `narrow_permissions`, `legacy_permissions_to_modes`, the
//!   `core_to_modes_*` translators). No fleet deps.
//! - This crate (fleet) — the concrete dispatcher impl wrapping
//!   [`aura_fleet_spawn::FleetSpawner`]. Renamed from the
//!   pre-refactor `RuntimeSubagentDispatch` because the surface crate
//!   it used to live in no longer owns it.
//!
//! All edges remain downward — no new entries in
//! `tests/layer_boundary.rs::WARN_ONLY_UPWARD_EDGES`.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
#![allow(
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
)]

pub mod dispatch;

pub use dispatch::FleetSubagentDispatcher;
