//! # aura-store-db
//!
//! Layer: store
//!
//! `RocksDB`-backed durable storage implementation for Aura.
//!
//! Phase 2 split: this crate is the renamed body of the legacy
//! `aura-store` crate. The original `aura-store` survives as a
//! re-export shell so existing call sites keep compiling.
//!
//! Provides:
//! - Column families for Record, Agent metadata, and Inbox
//! - Atomic commit protocol via `WriteBatch`
//! - Key encoding/decoding utilities
//! - A bridge from [`RocksStore`] to the
//!   [`aura_store_record::RecordLog`] trait for layered consumers.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod error;
mod keys;
mod record_log_bridge;
mod rocks_store;
pub mod seal;
mod store;

pub use aura_core_types::AgentStatus;
pub use error::StoreError;
pub use keys::{AgentMetaKey, InboxKey, KeyCodec, MetaField, RecordKey};
#[cfg(any(test, feature = "test-support"))]
pub use rocks_store::FaultAt;
pub use rocks_store::RocksStore;
pub use seal::{SealCipher, SealError, SEAL_MAGIC, SEAL_VERSION};
pub use store::{DequeueToken, ReadStore, Store, WriteStore};

/// Column family names.
pub mod cf {
    /// Record entries (append-only log per agent)
    pub const RECORD: &str = "record";
    /// Agent metadata (`head_seq`, status, etc.)
    pub const AGENT_META: &str = "agent_meta";
    /// Inbox (durable per-agent transaction queue)
    pub const INBOX: &str = "inbox";
    /// Memory: per-agent semantic facts
    pub const MEMORY_FACTS: &str = "memory_facts";
    /// Memory: per-agent episodic events
    pub const MEMORY_EVENTS: &str = "memory_events";
    /// Memory: per-agent procedural patterns
    pub const MEMORY_PROCEDURES: &str = "memory_procedures";
    /// Memory: event ID → timestamp secondary index
    pub const MEMORY_EVENT_INDEX: &str = "memory_event_index";
    /// Skill installations per agent
    pub const AGENT_SKILLS: &str = "agent_skills";
    /// Persisted runtime capability ledger per agent
    pub const RUNTIME_CAPABILITIES: &str = "runtime_capabilities";
    /// Per-user tool-permission default policy (full_access /
    /// auto_review / default_permissions). Keyed by `user_id` bytes;
    /// value is a JSON-serialised [`aura_core_types::UserToolDefaults`].
    pub const USER_TOOL_DEFAULTS: &str = "user_tool_defaults";
}
