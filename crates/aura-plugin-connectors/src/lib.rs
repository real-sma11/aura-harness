//! # aura-plugin-connectors
//!
//! Layer: plugin
//!
//! Connector registry for plugin-contributed external endpoints.
//! Phase 4c ships registration + lookup; no agent-loop wiring yet
//! (Phase 8 will land the consumer side that surfaces connectors to
//! tools / kernels).
//!
//! ## Surfaces
//!
//! - [`ConnectorEntry`] — opaque registry value carrying the
//!   connector id, contributing plugin id, and endpoint string.
//! - [`ConnectorRegistry`] — thread-safe in-process registry.
//!   Duplicate ids error with [`ConnectorError::AlreadyRegistered`].
//! - Error type: [`ConnectorError`].
//!
//! ## Invariants ([rules.md §13])
//!
//! - The registry uses a `BTreeMap` so [`ConnectorRegistry::list`]
//!   returns entries in deterministic id-sorted order — important
//!   for diagnostics (`aura plugins list` derivatives) and for
//!   future replay-determinism tests.
//! - Connector ids are global across the entire registry. Multiple
//!   plugins contributing the same id collide. The first contributor
//!   wins; the second registration returns
//!   [`ConnectorError::AlreadyRegistered`].

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod error;
pub mod registry;

pub use error::ConnectorError;
pub use registry::{ConnectorEntry, ConnectorRegistry};
