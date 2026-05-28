//! [`ParentLeaseRegistry`] — per-parent audit-append lease + Phase 7b
//! idempotent-spawn dedupe cache.
//!
//! Replaces the legacy single `spawn_lock` in
//! `crates/aura-runtime/src/subagent_dispatch.rs` (deleted in Phase
//! 7a). The old lock serialised EVERY spawn across the entire
//! daemon, so two unrelated parents could not spawn concurrently.
//! The per-parent lease keeps the in-order parent-side
//! `RecordEntry` append guarantee (concurrent spawns from one parent
//! still serialise) without the cross-parent contention.
//!
//! Phase 7b adds a small per-parent dedupe map keyed by the parent's
//! `tool_call_id`. When the same `(parent_id, tool_call_id)` is
//! spawned a second time within the dedupe window
//! ([`ParentLeaseRegistry::with_dedupe_window`], default 60s), the
//! second call returns a clone of the previous outcome reference
//! instead of producing a duplicate child.
//!
//! # Invariants
//!
//! - Each parent agent id maps to AT MOST ONE `Arc<Mutex<()>>`
//!   handle for the duration the registry observes any spawn for
//!   that parent.
//! - Holding a [`ParentLease`] guarantees mutual exclusion against
//!   any other spawn call for the same parent. The lock is held
//!   across the entire derive → quota → audit → registry → dispatch
//!   sequence so the parent's audit-record appends are linearised.
//! - The dedupe map is keyed by `(parent_id, tool_call_id)` — the
//!   tool_call_id is caller-supplied. Entries older than the dedupe
//!   window are evicted on the next access to keep the map bounded.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_core::{AgentId, SubagentResult};
use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Default window in which a `(parent_id, tool_call_id)` re-dispatch
/// is deduped to the previous outcome.
pub const DEFAULT_DEDUPE_WINDOW: Duration = Duration::from_secs(60);

/// RAII handle proving the holder has the parent's audit-append
/// lease. Dropping the handle releases the lease.
pub struct ParentLease {
    /// Held for the lease's lifetime; `_guard` is intentionally
    /// unread — its `Drop` is what releases the lock.
    _guard: OwnedMutexGuard<()>,
}

/// Cached outcome of a previous spawn — used by the dedupe path so a
/// repeated `(parent, tool_call_id)` returns the prior agent id /
/// result instead of producing a duplicate child.
#[derive(Debug, Clone)]
pub enum DedupedSpawn {
    /// Synchronous Wait-mode dedupe — clone the prior result.
    WaitResult(SubagentResult),
    /// Detached / Batch dedupe — return the prior child agent id(s).
    /// Detached carries one id; Batch carries one per child in spawn
    /// order.
    AgentIds(Vec<AgentId>),
}

#[derive(Debug, Clone)]
struct DedupeEntry {
    outcome: DedupedSpawn,
    inserted_at: Instant,
}

/// Per-parent lease pool with dedupe cache. Cloneable via `Arc` so
/// multiple call sites (task tool, mailbox dispatcher, SDK) share one
/// registry.
#[derive(Debug)]
pub struct ParentLeaseRegistry {
    /// `AgentId → shared lock`. Each parent gets its own lock; the
    /// dashmap-like outer lock is a `parking_lot::Mutex` only over
    /// the map mutation (fast — never held across `await`).
    inner: SyncMutex<HashMap<AgentId, Arc<AsyncMutex<()>>>>,
    /// `(parent, tool_call_id) → cached outcome`.
    dedupe: SyncMutex<HashMap<(AgentId, String), DedupeEntry>>,
    /// Window after which a dedupe entry is evicted.
    dedupe_window: Duration,
}

impl Default for ParentLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParentLeaseRegistry {
    /// Construct an empty registry with the default dedupe window
    /// ([`DEFAULT_DEDUPE_WINDOW`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_dedupe_window(DEFAULT_DEDUPE_WINDOW)
    }

    /// Construct an empty registry with an explicit dedupe window.
    /// Useful for tests that want to exercise the eviction path
    /// without sleeping for a minute.
    #[must_use]
    pub fn with_dedupe_window(window: Duration) -> Self {
        Self {
            inner: SyncMutex::new(HashMap::new()),
            dedupe: SyncMutex::new(HashMap::new()),
            dedupe_window: window,
        }
    }

    /// Acquire the lease for `parent`. Returns a [`ParentLease`]
    /// RAII guard that releases the lease on drop.
    ///
    /// Concurrent acquires for the same parent serialise; acquires
    /// for distinct parents proceed in parallel.
    pub async fn acquire(&self, parent: AgentId) -> ParentLease {
        let lock = {
            let mut map = self.inner.lock();
            map.entry(parent)
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = lock.lock_owned().await;
        ParentLease { _guard: guard }
    }

    /// Look up a prior outcome for `(parent, tool_call_id)`. Returns
    /// `Some(cached)` when a recently-completed spawn exists within
    /// the configured dedupe window. Expired entries are evicted
    /// during the lookup.
    pub fn lookup_dedupe(&self, parent: AgentId, tool_call_id: &str) -> Option<DedupedSpawn> {
        let mut map = self.dedupe.lock();
        // Evict-on-read so the map stays bounded without a separate
        // sweeper task. Each lookup pays O(map_size) in the worst
        // case; the map is intentionally tiny (one entry per pending
        // tool call per parent).
        let cutoff = Instant::now().checked_sub(self.dedupe_window);
        if let Some(cutoff) = cutoff {
            map.retain(|_, entry| entry.inserted_at >= cutoff);
        }
        let key = (parent, tool_call_id.to_string());
        map.get(&key).map(|entry| entry.outcome.clone())
    }

    /// Record `(parent, tool_call_id) -> outcome` in the dedupe
    /// cache. Subsequent calls within [`Self::dedupe_window`] for the
    /// same key receive a clone of `outcome`.
    pub fn record_dedupe(&self, parent: AgentId, tool_call_id: String, outcome: DedupedSpawn) {
        let mut map = self.dedupe.lock();
        map.insert(
            (parent, tool_call_id),
            DedupeEntry {
                outcome,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Configured dedupe window.
    #[must_use]
    pub fn dedupe_window(&self) -> Duration {
        self.dedupe_window
    }

    /// Snapshot count of distinct parents observed so far. Useful
    /// for tests + observability.
    #[must_use]
    pub fn known_parents(&self) -> usize {
        self.inner.lock().len()
    }

    /// Snapshot count of cached dedupe entries (post-eviction is the
    /// caller's job).
    #[must_use]
    pub fn dedupe_entries(&self) -> usize {
        self.dedupe.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn acquire_same_parent_serialises() {
        let registry = Arc::new(ParentLeaseRegistry::new());
        let parent = AgentId::generate();

        let a = registry.clone();
        let b = registry.clone();
        let start = Instant::now();
        let handle_a = tokio::spawn(async move {
            let _lease = a.acquire(parent).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        });
        let handle_b = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _lease = b.acquire(parent).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        });
        handle_a.await.unwrap();
        handle_b.await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(110),
            "expected serial execution (>=110ms), got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn acquire_distinct_parents_parallelises() {
        let registry = Arc::new(ParentLeaseRegistry::new());
        let parent_a = AgentId::generate();
        let parent_b = AgentId::generate();

        let a = registry.clone();
        let b = registry.clone();
        let start = Instant::now();
        let handle_a = tokio::spawn(async move {
            let _lease = a.acquire(parent_a).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
        });
        let handle_b = tokio::spawn(async move {
            let _lease = b.acquire(parent_b).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
        });
        handle_a.await.unwrap();
        handle_b.await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "expected parallel execution (<150ms), got {elapsed:?}"
        );
    }

    #[test]
    fn dedupe_returns_cached_outcome_within_window() {
        let registry = ParentLeaseRegistry::with_dedupe_window(Duration::from_secs(60));
        let parent = AgentId::generate();
        registry.record_dedupe(
            parent,
            "call-1".into(),
            DedupedSpawn::AgentIds(vec![AgentId::generate()]),
        );
        let cached = registry.lookup_dedupe(parent, "call-1");
        assert!(matches!(cached, Some(DedupedSpawn::AgentIds(ref v)) if v.len() == 1));
    }

    #[test]
    fn dedupe_evicts_expired_entries() {
        let registry = ParentLeaseRegistry::with_dedupe_window(Duration::from_millis(10));
        let parent = AgentId::generate();
        registry.record_dedupe(
            parent,
            "call-1".into(),
            DedupedSpawn::AgentIds(vec![AgentId::generate()]),
        );
        std::thread::sleep(Duration::from_millis(20));
        let cached = registry.lookup_dedupe(parent, "call-1");
        assert!(cached.is_none(), "expired entry must evict");
        assert_eq!(registry.dedupe_entries(), 0);
    }
}
