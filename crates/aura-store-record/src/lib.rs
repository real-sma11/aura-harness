//! # aura-store-record
//!
//! Layer: store
//!
//! Phase 2 home of the append-only per-agent record-log domain types
//! and the [`RecordLog`] trait. Today the heavyweight record-entry
//! struct ([`RecordEntry`], [`RecordEntryBuilder`], and the
//! [`KERNEL_VERSION`] constant) still physically lives in `aura-core`
//! and is re-exported here verbatim so the new layered home name
//! resolves. The new [`RecordKind`], [`RecordPayload`], and
//! [`RecordLog`] types are defined locally; they are the forward
//! shape that Phase 6+ migrates the rest of the workspace to.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - Per-agent record append is **linearisable**: a single writer
//!   succeeds at any given `(agent_id, seq)` slot, and observers see
//!   either the full append or no append (atomicity).
//! - Sequence numbers are **strictly monotone** per agent, with **no
//!   gaps** and **no duplicates**. The caller (kernel today; the
//!   per-parent audit-append lease in Phase 7a) is responsible for
//!   computing `seq = head_seq + 1` before issuing
//!   [`RecordLog::append`].
//! - Implementations MUST reject any append whose `seq` is not the
//!   next expected value for the agent by returning
//!   [`RecordLogError::SeqOutOfOrder`].
//!
//! ## Assumptions
//!
//! - Storage-backend errors (RocksDB I/O, network filesystems, etc.)
//!   are reported via [`RecordLogError::Backend`] with a human-
//!   readable message; structured backend errors live in the
//!   concrete-impl crate (e.g. `aura-store-db::StoreError`).
//! - The kernel is the sole producer of [`RecordEntry`] values today.
//!   Phase 6a introduces [`RecordKind::PermissionsUpdate`] to close
//!   the only remaining non-kernel write path (today's
//!   `aura-runtime::tool_permissions` direct append).
//!
//! ## Failure modes
//!
//! - [`RecordLogError::Backend`] — storage failure (disk full,
//!   corruption, transient I/O); the caller is expected to retry or
//!   surface to the user.
//! - [`RecordLogError::SeqOutOfOrder`] — caller computed the wrong
//!   next sequence; this is a logic bug, never a transient condition.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod kind;
mod log;
mod payload;
mod record;

pub use kind::RecordKind;
pub use log::{RecordLog, RecordLogError};
pub use payload::{summarize_payload, RecordPayload, DEFAULT_SUMMARY_CHUNK_BYTES};
pub use record::{RecordEntry, RecordEntryBuilder, KERNEL_VERSION};

/// Schema version of the on-disk record / transaction wire format.
///
/// Phase 10 bump (v1 → v2) consolidates three closed-enum additions:
///
/// - [`RecordKind::SessionStop`] — session-lifecycle audit row
///   emitted by the fleet daemon's shutdown path.
/// - [`RecordKind::ToolCallBlockedByHook`] — audit row replacing
///   `ToolCallResult` when a `PreToolUse` hook blocks dispatch.
/// - [`aura_core::TransactionType::SubagentSpawn`] — typed wire
///   variant replacing the Phase 7a `TransactionType::System +
///   "kind": "subagent_spawn"` JSON-discriminator workaround.
///
/// **Migration policy**: pre-bump (v1) records continue to parse
/// without churn — every unknown `RecordKind` tag falls through to
/// [`RecordKind::Unknown`] (Phase 2 forward-compat variant) so an
/// older binary reading a v2 log gracefully degrades on the new
/// variants. The kernel WRITES only v2 going forward. Operators
/// rolling back a binary must accept that v2-only audit rows
/// (`SessionStop`, `ToolCallBlockedByHook`, `SubagentSpawn`) will
/// appear as `Unknown` instead of being lost.
///
/// See `CHANGELOG.md` Phase 10 entry for the operator-facing
/// migration note.
pub const SCHEMA_VERSION: u32 = 2;

#[cfg(test)]
mod tests;
