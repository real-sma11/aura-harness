//! # aura-fleet-quota
//!
//! Layer: fleet
//!
//! Concurrency and resource budgets across the fleet.
//!
//! ## Phase 7b — real enforcement with RAII tickets
//!
//! `QuotaPool::try_acquire` now enforces four caps:
//!
//! 1. `max_concurrent_per_parent` — outstanding child slots per parent
//!    agent.
//! 2. `max_concurrent_global` — outstanding child slots across the
//!    entire fleet.
//! 3. `max_depth` — refuses any spawn whose depth (child depth =
//!    `parent_depth + 1`) would exceed the configured ceiling. Depth
//!    is also re-checked inside `aura-agent-subagent::derive_subagent`
//!    for layering reasons; the quota copy here lets call sites
//!    short-circuit before deriving when a [`QuotaRequest`] carries an
//!    explicit `child_depth`.
//! 4. `tokens_per_minute_per_parent` — a simple bucket rate limiter
//!    keyed by parent agent id. Optional; when `None` the limiter is
//!    disabled.
//!
//! [`BudgetTicket`] is an RAII handle: when the parent's child loop
//!  completes the ticket is dropped and the pool's per-parent +
//!  global counters decrement automatically. The ticket carries an
//!  internal `Arc<QuotaPoolInner>` reference so the drop hook can
//!  release without an explicit `release(ticket)` call. The legacy
//!  `release(ticket)` shape is removed — Drop is the single release
//!  surface so call sites never leak a slot via an early `?`.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - Every successful `try_acquire` returns a unique
//!   [`BudgetTicket::ticket_id`] (UUID v4) — useful for correlating
//!   across spawn / dispatch / audit logs.
//! - Counters increment AFTER the cap check passes; never speculatively.
//! - Counters decrement EXACTLY ONCE per ticket — via the Drop hook.
//!   Cloning a `BudgetTicket` is intentionally NOT supported so the
//!   single-release invariant is type-enforced.
//! - The rate limiter uses a sub-second-resolution token bucket per
//!   parent (replenished proportionally on each acquire); refusing a
//!   spawn returns the configured cap so callers can format a
//!   user-visible error.
//!
//! ## Assumptions
//!
//! - Tickets are dropped promptly when the child loop exits. The
//!   fleet-spawn pipeline holds the ticket inside its dispatch
//!   future so cancellation / panic / natural completion all release.
//! - The pool is thread-safe (`parking_lot::Mutex` inside the inner
//!   accounting struct).
//!
//! ## Failure modes
//!
//! - [`QuotaError::TooManyConcurrentForParent`] — per-parent cap hit.
//! - [`QuotaError::TooManyConcurrentGlobal`] — global cap hit.
//! - [`QuotaError::DepthExceeded`] — child depth would exceed the
//!   `max_depth` ceiling.
//! - [`QuotaError::RateLimited`] — per-parent token bucket exhausted.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_core::AgentId;
use parking_lot::Mutex;
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

/// Static configuration for a [`QuotaPool`]. Read-only after
/// construction.
#[derive(Debug, Clone, Copy)]
pub struct QuotaConfig {
    /// Hard cap on outstanding children per parent.
    pub max_concurrent_per_parent: usize,
    /// Hard cap on outstanding children across the entire fleet.
    pub max_concurrent_global: usize,
    /// Hard cap on subagent depth (child depth = parent depth + 1).
    pub max_depth: u8,
    /// Optional per-parent rate-limit ceiling in spawn-tokens / minute.
    /// `None` disables rate limiting.
    pub tokens_per_minute_per_parent: Option<u64>,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_parent: 8,
            max_concurrent_global: 64,
            max_depth: 8,
            tokens_per_minute_per_parent: None,
        }
    }
}

/// Requested per-spawn budget.
#[derive(Debug, Clone, Copy)]
pub struct QuotaRequest {
    /// Parent agent the budget is charged to.
    pub agent_id: AgentId,
    /// Resolved child depth (parent_depth + 1). Used to enforce
    /// [`QuotaConfig::max_depth`].
    pub child_depth: u8,
    /// Requested max iteration ceiling. Recorded for observability.
    pub max_iterations: u32,
    /// Requested concurrent-tool ceiling. Recorded for observability.
    pub max_concurrent_tools: u32,
    /// Optional token budget. Recorded for observability.
    pub token_budget: Option<u64>,
}

/// RAII ticket returned from [`QuotaPool::try_acquire`]. The pool's
/// counters release when this value is dropped — no explicit
/// `release` call is required.
#[derive(Debug)]
pub struct BudgetTicket {
    /// Unique correlation id for this acquire.
    pub ticket_id: Uuid,
    /// Parent agent the ticket was issued to.
    pub agent_id: AgentId,
    /// Recorded max iterations (mirrors [`QuotaRequest::max_iterations`]).
    pub max_iterations: u32,
    /// Recorded concurrent-tool cap.
    pub max_concurrent_tools: u32,
    /// Recorded optional token budget.
    pub token_budget: Option<u64>,
    /// Shared pool handle used to release the per-parent + global
    /// counters on drop. `None` means the ticket has already been
    /// released (used by tests and the swap-trick in some callsites).
    pool: Option<Arc<QuotaPoolInner>>,
}

impl Drop for BudgetTicket {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(self.agent_id);
        }
    }
}

/// Errors returned by [`QuotaPool::try_acquire`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum QuotaError {
    /// Per-parent concurrency ceiling reached.
    #[error(
        "quota: parent {agent_id} already has {current} concurrent children (max {max} per parent)"
    )]
    TooManyConcurrentForParent {
        /// Parent agent id.
        agent_id: AgentId,
        /// Current outstanding count.
        current: usize,
        /// Configured per-parent cap.
        max: usize,
    },
    /// Global concurrency ceiling reached.
    #[error("quota: fleet already has {current} concurrent children (max {max} global)")]
    TooManyConcurrentGlobal {
        /// Current global outstanding count.
        current: usize,
        /// Configured global cap.
        max: usize,
    },
    /// Child depth would exceed the configured maximum.
    #[error("quota: child depth {child_depth} exceeds max depth {max_depth}")]
    DepthExceeded {
        /// Requested child depth.
        child_depth: u8,
        /// Configured ceiling.
        max_depth: u8,
    },
    /// Per-parent rate limiter exhausted.
    #[error(
        "quota: parent {agent_id} hit rate limit ({max_per_minute} spawn-tokens / minute); \
         retry after {retry_after_ms}ms"
    )]
    RateLimited {
        /// Parent agent id.
        agent_id: AgentId,
        /// Configured rate limit.
        max_per_minute: u64,
        /// Estimated milliseconds the caller should wait before
        /// retrying.
        retry_after_ms: u64,
    },
}

/// Inner mutable state of [`QuotaPool`]. Separated so that
/// [`BudgetTicket`] can hold an `Arc` reference to it without
/// `BudgetTicket` having to drag the full public surface around.
#[derive(Debug)]
pub(crate) struct QuotaPoolInner {
    config: QuotaConfig,
    state: Mutex<QuotaState>,
}

#[derive(Debug, Default)]
struct QuotaState {
    /// Outstanding children keyed by parent agent.
    outstanding_per_parent: HashMap<AgentId, usize>,
    /// Outstanding children across the entire fleet.
    outstanding_global: usize,
    /// Rate-limit token bucket state keyed by parent agent.
    rate_buckets: HashMap<AgentId, RateBucket>,
}

#[derive(Debug, Clone, Copy)]
struct RateBucket {
    /// Tokens currently available (fractional accounting tracked
    /// in `tokens`).
    tokens: f64,
    /// Wall-clock instant of the most recent acquire/refill.
    last_refill: Instant,
}

impl QuotaPoolInner {
    fn release(&self, parent: AgentId) {
        let mut state = self.state.lock();
        if let Some(slot) = state.outstanding_per_parent.get_mut(&parent) {
            *slot = slot.saturating_sub(1);
            if *slot == 0 {
                state.outstanding_per_parent.remove(&parent);
            }
        }
        state.outstanding_global = state.outstanding_global.saturating_sub(1);
        debug!(
            parent = %parent,
            global = state.outstanding_global,
            "quota pool: ticket released"
        );
    }
}

/// Concurrency / resource budget pool with real enforcement.
#[derive(Debug, Clone)]
pub struct QuotaPool {
    inner: Arc<QuotaPoolInner>,
}

impl Default for QuotaPool {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaPool {
    /// Construct a pool with the Phase 7b default [`QuotaConfig`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(QuotaConfig::default())
    }

    /// Construct a pool with an explicit config.
    #[must_use]
    pub fn with_config(config: QuotaConfig) -> Self {
        Self {
            inner: Arc::new(QuotaPoolInner {
                config,
                state: Mutex::new(QuotaState::default()),
            }),
        }
    }

    /// Read-only snapshot of the pool's static configuration.
    #[must_use]
    pub fn config(&self) -> &QuotaConfig {
        &self.inner.config
    }

    /// Attempt to acquire a ticket. Returns a typed
    /// [`QuotaError`] when any of the four caps would be exceeded;
    /// otherwise hands back an RAII [`BudgetTicket`] whose Drop hook
    /// decrements the counters.
    ///
    /// # Errors
    ///
    /// See the [`QuotaError`] variants.
    pub fn try_acquire(&self, request: QuotaRequest) -> Result<BudgetTicket, QuotaError> {
        let config = self.inner.config;

        if request.child_depth > config.max_depth {
            return Err(QuotaError::DepthExceeded {
                child_depth: request.child_depth,
                max_depth: config.max_depth,
            });
        }

        let mut state = self.inner.state.lock();

        let per_parent = state
            .outstanding_per_parent
            .get(&request.agent_id)
            .copied()
            .unwrap_or(0);
        if per_parent >= config.max_concurrent_per_parent {
            return Err(QuotaError::TooManyConcurrentForParent {
                agent_id: request.agent_id,
                current: per_parent,
                max: config.max_concurrent_per_parent,
            });
        }
        if state.outstanding_global >= config.max_concurrent_global {
            return Err(QuotaError::TooManyConcurrentGlobal {
                current: state.outstanding_global,
                max: config.max_concurrent_global,
            });
        }

        if let Some(limit) = config.tokens_per_minute_per_parent {
            check_rate_limit(&mut state.rate_buckets, request.agent_id, limit)?;
        }

        state
            .outstanding_per_parent
            .entry(request.agent_id)
            .and_modify(|n| *n += 1)
            .or_insert(1);
        state.outstanding_global += 1;
        let global = state.outstanding_global;
        drop(state);

        let ticket = BudgetTicket {
            ticket_id: Uuid::new_v4(),
            agent_id: request.agent_id,
            max_iterations: request.max_iterations,
            max_concurrent_tools: request.max_concurrent_tools,
            token_budget: request.token_budget,
            pool: Some(self.inner.clone()),
        };
        debug!(
            ticket_id = %ticket.ticket_id,
            agent_id = %ticket.agent_id,
            per_parent = per_parent + 1,
            global,
            "quota pool: ticket acquired"
        );
        Ok(ticket)
    }

    /// Snapshot count of outstanding tickets globally.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.inner.state.lock().outstanding_global
    }

    /// Snapshot count of outstanding tickets for a single parent.
    #[must_use]
    pub fn outstanding_for(&self, parent: AgentId) -> usize {
        self.inner
            .state
            .lock()
            .outstanding_per_parent
            .get(&parent)
            .copied()
            .unwrap_or(0)
    }
}

/// Refill the parent's bucket pro-rata, then deduct one token. The
/// bucket capacity equals the per-minute limit so a fresh parent can
/// burst up to `limit` spawns before the limiter engages.
fn check_rate_limit(
    buckets: &mut HashMap<AgentId, RateBucket>,
    parent: AgentId,
    limit: u64,
) -> Result<(), QuotaError> {
    let now = Instant::now();
    #[allow(clippy::cast_precision_loss)]
    let limit_f = limit as f64;
    let bucket = buckets.entry(parent).or_insert(RateBucket {
        tokens: limit_f,
        last_refill: now,
    });
    let elapsed = now.saturating_duration_since(bucket.last_refill);
    let refill = (elapsed.as_secs_f64() / 60.0) * limit_f;
    bucket.tokens = (bucket.tokens + refill).min(limit_f);
    bucket.last_refill = now;
    if bucket.tokens < 1.0 {
        let needed = 1.0 - bucket.tokens;
        let retry_secs = (needed / limit_f) * 60.0;
        let retry_after = Duration::from_secs_f64(retry_secs.max(0.0));
        let retry_after_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
        warn!(
            parent = %parent,
            tokens = bucket.tokens,
            retry_after_ms,
            "quota pool: rate limit hit"
        );
        return Err(QuotaError::RateLimited {
            agent_id: parent,
            max_per_minute: limit,
            retry_after_ms,
        });
    }
    bucket.tokens -= 1.0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_at_depth(agent_id: AgentId, depth: u8) -> QuotaRequest {
        QuotaRequest {
            agent_id,
            child_depth: depth,
            max_iterations: 50,
            max_concurrent_tools: 4,
            token_budget: Some(64_000),
        }
    }

    #[test]
    fn default_pool_acquires_and_releases() {
        let pool = QuotaPool::new();
        let id = AgentId::generate();
        {
            let _ticket = pool.try_acquire(request_at_depth(id, 1)).expect("ok");
            assert_eq!(pool.outstanding(), 1);
            assert_eq!(pool.outstanding_for(id), 1);
        }
        assert_eq!(pool.outstanding(), 0, "ticket drop must release");
        assert_eq!(pool.outstanding_for(id), 0);
    }

    #[test]
    fn per_parent_cap_rejects_excess() {
        let config = QuotaConfig {
            max_concurrent_per_parent: 2,
            max_concurrent_global: 1024,
            max_depth: 8,
            tokens_per_minute_per_parent: None,
        };
        let pool = QuotaPool::with_config(config);
        let id = AgentId::generate();
        let _a = pool.try_acquire(request_at_depth(id, 1)).unwrap();
        let _b = pool.try_acquire(request_at_depth(id, 1)).unwrap();
        let err = pool.try_acquire(request_at_depth(id, 1)).unwrap_err();
        assert!(matches!(
            err,
            QuotaError::TooManyConcurrentForParent {
                current: 2,
                max: 2,
                ..
            }
        ));
    }

    #[test]
    fn global_cap_rejects_excess() {
        let config = QuotaConfig {
            max_concurrent_per_parent: 1024,
            max_concurrent_global: 1,
            max_depth: 8,
            tokens_per_minute_per_parent: None,
        };
        let pool = QuotaPool::with_config(config);
        let id_a = AgentId::generate();
        let id_b = AgentId::generate();
        let _a = pool.try_acquire(request_at_depth(id_a, 1)).unwrap();
        let err = pool.try_acquire(request_at_depth(id_b, 1)).unwrap_err();
        assert!(matches!(
            err,
            QuotaError::TooManyConcurrentGlobal { current: 1, max: 1 }
        ));
    }

    #[test]
    fn depth_cap_rejects_too_deep() {
        let config = QuotaConfig {
            max_concurrent_per_parent: 8,
            max_concurrent_global: 64,
            max_depth: 2,
            tokens_per_minute_per_parent: None,
        };
        let pool = QuotaPool::with_config(config);
        let id = AgentId::generate();
        let err = pool.try_acquire(request_at_depth(id, 3)).unwrap_err();
        assert!(matches!(
            err,
            QuotaError::DepthExceeded {
                child_depth: 3,
                max_depth: 2
            }
        ));
        assert_eq!(pool.outstanding(), 0);
    }

    #[test]
    fn rate_limit_engages_after_burst() {
        let config = QuotaConfig {
            max_concurrent_per_parent: 1024,
            max_concurrent_global: 1024,
            max_depth: 8,
            tokens_per_minute_per_parent: Some(2),
        };
        let pool = QuotaPool::with_config(config);
        let id = AgentId::generate();
        let _a = pool.try_acquire(request_at_depth(id, 1)).unwrap();
        let _b = pool.try_acquire(request_at_depth(id, 1)).unwrap();
        let err = pool.try_acquire(request_at_depth(id, 1)).unwrap_err();
        assert!(matches!(
            err,
            QuotaError::RateLimited {
                max_per_minute: 2,
                ..
            }
        ));
    }

    #[test]
    fn distinct_parents_acquire_independently() {
        let pool = QuotaPool::new();
        let a = AgentId::generate();
        let b = AgentId::generate();
        let _ta = pool.try_acquire(request_at_depth(a, 1)).unwrap();
        let _tb = pool.try_acquire(request_at_depth(b, 1)).unwrap();
        assert_eq!(pool.outstanding_for(a), 1);
        assert_eq!(pool.outstanding_for(b), 1);
        assert_eq!(pool.outstanding(), 2);
    }
}
