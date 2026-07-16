//! Per-endpoint handler bundles for the gateway.
//!
//! Each submodule owns the HTTP / WS handler functions for one
//! endpoint family. The route mounting happens in
//! [`super::middleware::create_router`].
//!
//! Phase C / Commit 4 introduced this grouping (handlers were
//! scattered at the top level of the former `router/` module
//! pre-rename).
//!
//! ## Layout
//!
//! - [`run`] — `POST /v1/run` + `/v1/run/list` + per-run lifecycle
//!   (`status` / `pause` / `stop`). Canonical entry for chat / dev-loop /
//!   task-run kickoffs.
//! - [`run_ws`] — `WS /stream/:run_id` upgrade and event-only
//!   forwarder for DevLoop / TaskRun automatons.
//! - [`files`] — `/api/files`, `/api/read-file`, and hosted workspace
//!   resolve/import/delete lifecycle endpoints.
//! - [`tx`] — `/tx`, `/tx/status/:agent_id/:tx_id`, `/agents/:id/head`,
//!   `/agents/:id/record`.
//! - [`memory`] — memory CRUD (canonical paths + aura-os `/api/*`
//!   compatibility aliases).
//! - [`skills`] — skill CRUD + per-agent install/uninstall.
//! - [`secrets`] — in-TEE secrets vault CRUD (`/secrets`,
//!   `/secrets/:name`; Swarm TEE phase 6).
//! - [`processes`] — in-TEE process / automation CRUD + trigger
//!   (`/v1/processes`; Swarm TEE phase 7).
//! - [`tool_permissions`] — user defaults + per-agent overrides.
//! - [`util`] — shared parsing helpers (`parse_agent_id` for the
//!   bare/partitioned UUID + hex grammars).
//!
//! Each handler module pulls common dependencies via `use super::*;`
//! which re-globs the broad `use` block at [`super`] (the
//! `gateway` module root). Items there are private to the gateway
//! and visible to children via name resolution; the
//! [`pub(in crate::gateway) use`] re-export below keeps the same shape working
//! from the new `handlers/` subtree.

pub(crate) mod files;
pub(crate) mod memory;
pub(crate) mod processes;
pub(crate) mod run;
pub(crate) mod run_ws;
pub(crate) mod secrets;
pub(crate) mod skills;
pub(crate) mod tool_permissions;
pub(crate) mod tx;
pub(crate) mod util;
