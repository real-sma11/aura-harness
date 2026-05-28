//! # aura-exec-conflict
//!
//! Layer: exec
//!
//! Domain-scoped advisory locks. NOT the primary safety mechanism for
//! parallel execution — isolation ([`aura-exec-isolation`]) is. This
//! crate exists to avoid avoidable conflicts when two siblings happen
//! to target the same domain (e.g., same file path, same git
//! operation, same remote host).
//!
//! ## Invariants ([rules.md §13])
//!
//! - Locks are **advisory**: the kernel still records every operation
//!   and isolation provides the hard safety guarantee. A lock
//!   acquisition failure does not invalidate state.
//! - Default wait policy is bounded by the caller's [`Duration`]; a
//!   timeout returns [`ConflictError::Timeout`] rather than blocking
//!   the runtime indefinitely. The conventional default budget lives in
//!   `aura_config::ConflictConfig::default_wait_ms` and is wired by
//!   [`aura-exec-runner`] when it acquires on behalf of a tool call.
//! - [`ConflictDomain`] is hashable + serialisable so future phases
//!   can audit-log lock acquisitions through the kernel record log.
//! - The registry holds a [`std::sync::Mutex`] over the domain map and
//!   a per-domain [`tokio::sync::Mutex`]. Holding the std mutex is
//!   strictly bounded to the `entry().or_insert_with()` insert, so the
//!   std mutex is never held across an `.await`.
//!
//! ## Failure modes
//!
//! - [`ConflictError::Timeout`] — the wait budget elapsed before the
//!   per-domain lock became free. Callers can retry, escalate, or
//!   surface the conflict to the operator.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Logical domain that a tool call wants to mutate.
///
/// Two calls that target the same [`ConflictDomain`] should serialise
/// at the [`ConflictRegistry`]; calls that target distinct domains
/// proceed concurrently. The variants are intentionally coarse — the
/// goal is to avoid obvious collisions, not to model every possible
/// resource granularity. Future variants can be added behind a feature
/// flag without breaking wire format because the enum is externally
/// tagged via `#[serde(rename_all = "snake_case")]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDomain {
    /// A filesystem path (file or directory). Two operations that
    /// touch the same path serialise.
    Path(PathBuf),
    /// A logical git operation name (e.g. `"commit"`, `"push"`,
    /// `"add"`). Different repositories use the same name; pair with a
    /// repo-qualified [`Self::Path`] grant on the caller's side if
    /// finer scoping is required.
    GitOp(String),
    /// A network host (DNS name or `host:port`).
    NetworkHost(String),
}

/// Errors returned by [`ConflictRegistry::acquire`].
#[derive(Debug, Error)]
pub enum ConflictError {
    /// The lock was not free within the caller-supplied wait budget.
    /// Carries the contended [`ConflictDomain`] for diagnostics.
    #[error("timeout acquiring conflict lock for {0:?}")]
    Timeout(ConflictDomain),
}

/// Process-wide registry of per-[`ConflictDomain`] advisory locks.
///
/// Acquire a guard with [`Self::acquire`]; release happens
/// automatically when the returned [`LockHandle`] is dropped. The
/// registry never evicts entries — repeated acquisitions on the same
/// domain reuse the same `Arc<AsyncMutex<()>>`, so guard ordering is
/// stable across siblings.
#[derive(Default)]
pub struct ConflictRegistry {
    locks: Mutex<HashMap<ConflictDomain, Arc<AsyncMutex<()>>>>,
}

impl std::fmt::Debug for ConflictRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner mutex map is intentionally opaque — its contents
        // can mutate from another task while the formatter holds the
        // std lock, and the locks themselves are not Debug.
        f.debug_struct("ConflictRegistry").finish_non_exhaustive()
    }
}

impl ConflictRegistry {
    /// Construct a fresh registry. No background work is started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the advisory lock for `domain`, waiting up to `wait`.
    ///
    /// Returns a [`LockHandle`] that releases the lock when dropped.
    /// If the wait budget elapses before the lock becomes free,
    /// returns [`ConflictError::Timeout`] containing `domain`.
    ///
    /// # Errors
    ///
    /// - [`ConflictError::Timeout`] when `wait` elapsed.
    ///
    /// # Panics
    ///
    /// Panics only if the internal std `Mutex` is poisoned — i.e. a
    /// previous holder panicked while the map was locked. This is a
    /// process-wide invariant violation; the registry cannot meaningfully
    /// continue after one.
    pub async fn acquire(
        &self,
        domain: ConflictDomain,
        wait: Duration,
    ) -> Result<LockHandle, ConflictError> {
        let mu = {
            let mut guard = self
                .locks
                .lock()
                .expect("ConflictRegistry inner mutex poisoned");
            Arc::clone(
                guard
                    .entry(domain.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        let guard = tokio::time::timeout(wait, mu.lock_owned())
            .await
            .map_err(|_| ConflictError::Timeout(domain))?;
        Ok(LockHandle { _guard: guard })
    }
}

/// RAII guard returned by [`ConflictRegistry::acquire`]. The lock is
/// released when this value is dropped.
pub struct LockHandle {
    _guard: OwnedMutexGuard<()>,
}

impl std::fmt::Debug for LockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockHandle").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_domain_roundtrips() {
        let domain = ConflictDomain::Path(PathBuf::from("/tmp/x"));
        let s = serde_json::to_string(&domain).unwrap();
        let back: ConflictDomain = serde_json::from_str(&s).unwrap();
        assert_eq!(domain, back);
    }

    #[test]
    fn conflict_domain_variants_are_distinct() {
        let p = ConflictDomain::Path(PathBuf::from("/tmp/x"));
        let g = ConflictDomain::GitOp("push".into());
        let n = ConflictDomain::NetworkHost("example.com".into());
        assert_ne!(p, g);
        assert_ne!(g, n);
        assert_ne!(p, n);
    }
}
