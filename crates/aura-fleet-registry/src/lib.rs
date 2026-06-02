//! # aura-fleet-registry
//!
//! Layer: fleet
//!
//! In-memory directory of every agent the local fleet daemon knows
//! about — both live (root + subagents currently running on the
//! daemon's runtime) and recently-terminated (`Done` / `Failed` /
//! `Cancelled`) entries the dispatch and audit layers may still need
//! to reference.
//!
//! Phase 7a ships the minimum viable surface the new
//! `aura-fleet-spawn` adapter needs to record a subagent at spawn
//! time and let other crates (notably `aura-fleet-dispatch` and the
//! task-tool compat adapter) look the child up by id. The richer
//! design from Section 4 of the architecture plan (parent/child
//! tree index, lifecycle event stream, persisted snapshot) lands in
//! Phase 7b — `FleetRegistry` is the foundation that survives.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - Each [`AgentSlot`] is uniquely keyed by its [`aura_core_types::AgentId`].
//!   [`FleetRegistry::register`] rejects a duplicate id with
//!   [`RegistryError::AlreadyRegistered`] so the caller never
//!   silently clobbers an existing slot.
//! - [`AgentSlot::parent_id`] establishes a single-parent tree
//!   relationship; cycles are impossible because [`AgentId`] values
//!   are u128-uniform and only assigned on a fresh registration.
//! - The registry holds NO `RwLock` across an `await` point — every
//!   public method acquires the lock, mutates / clones, and returns
//!   before yielding. Callers may freely await on returned values.
//!
//! ## Assumptions
//!
//! - The fleet daemon owns the single `Arc<FleetRegistry>`; surface
//!   crates obtain it through `aura-fleet-daemon::FleetDaemon::handle`.
//! - The registry does NOT persist; a daemon restart starts with an
//!   empty registry. Persistent agent state lives in
//!   `aura-store-record` and is reconstituted lazily.
//! - [`AgentState`] transitions are advisory in Phase 7a — callers
//!   are expected to call [`FleetRegistry::set_state`] before the
//!   slot leaves the live set, but the registry does not enforce
//!   ordering or terminal-state immutability yet. Phase 7b adds a
//!   strict lifecycle FSM.
//!
//! ## Failure modes
//!
//! - [`RegistryError::AlreadyRegistered`] — duplicate `agent_id`
//!   passed to `register`; the caller is expected to surface this as
//!   a logic bug (fresh ids must be generated for each spawn).
//! - [`RegistryError::NotFound`] — `set_state` / `unregister` called
//!   for an id with no live slot; the caller is expected to ignore
//!   or log the race.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::collections::HashMap;

use aura_core_modes::{AgentMode, KernelMode};
use aura_core_permissions::Permissions;
use aura_core_types::AgentId;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use thiserror::Error;
use tracing::debug;

/// Lifecycle phase of an [`AgentSlot`].
///
/// Phase 7a uses these tags only for observability — terminal-state
/// immutability is enforced by callers, not by the registry. Phase
/// 7b promotes this to a strict FSM and validates every transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    /// Agent loop is executing (root agent in-session, or subagent
    /// inside its turn loop).
    Running,
    /// Agent loop returned successfully.
    Done,
    /// Agent loop returned with an error.
    Failed,
    /// Cooperative cancellation completed (parent dropped, quota
    /// withdrawn, user `aura agents cancel`).
    Cancelled,
}

/// Per-agent fleet metadata. One slot exists per registered agent
/// for the duration of that agent's local visibility on the daemon.
#[derive(Debug, Clone)]
pub struct AgentSlot {
    /// Stable agent identifier.
    pub agent_id: AgentId,
    /// Parent agent id when this slot was registered as a subagent.
    /// `None` for root agents.
    pub parent_id: Option<AgentId>,
    /// Agent's resolved [`AgentMode`].
    pub mode: AgentMode,
    /// Agent's [`KernelMode`] tier (informs payload retention).
    pub kernel_mode: KernelMode,
    /// Agent's effective [`Permissions`] at registration time
    /// (mode-intersected by `aura-agent-subagent`).
    pub permissions: Permissions,
    /// Wall-clock instant the slot was registered (UTC).
    pub started_at: DateTime<Utc>,
    /// Current lifecycle phase.
    pub state: AgentState,
}

impl AgentSlot {
    /// Construct a freshly-registered slot in [`AgentState::Running`]
    /// at the current UTC instant.
    #[must_use]
    pub fn new(
        agent_id: AgentId,
        parent_id: Option<AgentId>,
        mode: AgentMode,
        kernel_mode: KernelMode,
        permissions: Permissions,
    ) -> Self {
        Self {
            agent_id,
            parent_id,
            mode,
            kernel_mode,
            permissions,
            started_at: Utc::now(),
            state: AgentState::Running,
        }
    }
}

/// Errors returned by [`FleetRegistry`] mutators.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    /// `register` was called for an id that already has a slot.
    /// Callers must always allocate a fresh [`AgentId`] for each
    /// spawn — receiving this variant indicates a logic bug.
    #[error("agent_id {0} already registered")]
    AlreadyRegistered(AgentId),

    /// `set_state` / `unregister` referenced an unknown id. This
    /// usually indicates a race with `unregister` and is safe to
    /// log + ignore.
    #[error("agent_id {0} not found in registry")]
    NotFound(AgentId),
}

/// In-memory map `AgentId → AgentSlot` plus a parent index for
/// `children_of`. Wrapped in a single `parking_lot::RwLock` so the
/// hot read path (`get`, `children_of`) takes the shared lock and
/// only the rare mutators take exclusive.
#[derive(Debug, Default)]
pub struct FleetRegistry {
    inner: RwLock<HashMap<AgentId, AgentSlot>>,
}

impl FleetRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly-spawned agent slot.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::AlreadyRegistered`] if a slot with
    /// the same [`AgentId`] is already present.
    pub fn register(&self, slot: AgentSlot) -> Result<(), RegistryError> {
        let mut guard = self.inner.write();
        if guard.contains_key(&slot.agent_id) {
            return Err(RegistryError::AlreadyRegistered(slot.agent_id));
        }
        debug!(
            agent_id = %slot.agent_id,
            parent_id = ?slot.parent_id,
            mode = ?slot.mode,
            kernel_mode = ?slot.kernel_mode,
            "fleet registry: register slot"
        );
        guard.insert(slot.agent_id, slot);
        Ok(())
    }

    /// Remove a slot. Used by `aura-fleet-dispatch` (Phase 7b+) when
    /// a terminal-state agent ages out of the live set.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotFound`] if no slot existed for
    /// `agent_id`.
    pub fn unregister(&self, agent_id: AgentId) -> Result<(), RegistryError> {
        let mut guard = self.inner.write();
        if guard.remove(&agent_id).is_some() {
            debug!(agent_id = %agent_id, "fleet registry: unregister slot");
            Ok(())
        } else {
            Err(RegistryError::NotFound(agent_id))
        }
    }

    /// Update the lifecycle state of an existing slot.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotFound`] if no slot existed for
    /// `agent_id`.
    pub fn set_state(&self, agent_id: AgentId, state: AgentState) -> Result<(), RegistryError> {
        let mut guard = self.inner.write();
        let Some(slot) = guard.get_mut(&agent_id) else {
            return Err(RegistryError::NotFound(agent_id));
        };
        slot.state = state;
        Ok(())
    }

    /// Look up a slot by id. Returns a clone so the caller can use
    /// the slot data across `await` points without holding the lock.
    #[must_use]
    pub fn get(&self, agent_id: AgentId) -> Option<AgentSlot> {
        self.inner.read().get(&agent_id).cloned()
    }

    /// Return the ids of every slot whose `parent_id == Some(parent)`.
    /// Order is unspecified — the caller is expected to sort if a
    /// stable order matters.
    #[must_use]
    pub fn children_of(&self, parent: AgentId) -> Vec<AgentId> {
        self.inner
            .read()
            .values()
            .filter(|slot| slot.parent_id == Some(parent))
            .map(|slot| slot.agent_id)
            .collect()
    }

    /// Snapshot count of currently-registered slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// True iff no slots are currently registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Number of slots whose lifecycle phase is
    /// [`AgentState::Running`]. Phase 10 carve-out 4 polls this
    /// from the fleet daemon's shutdown sequence to decide when
    /// the grace window can end early (count == 0) and to emit
    /// `clean_shutdown: false` when the grace window expires
    /// while the count is still > 0.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.inner
            .read()
            .values()
            .filter(|slot| slot.state == AgentState::Running)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core_modes::AgentMode;

    fn slot(agent_id: AgentId, parent: Option<AgentId>) -> AgentSlot {
        AgentSlot::new(
            agent_id,
            parent,
            AgentMode::Agent,
            KernelMode::Audited,
            Permissions::empty(),
        )
    }

    #[test]
    fn register_then_get_returns_slot() {
        let registry = FleetRegistry::new();
        let id = AgentId::generate();
        registry.register(slot(id, None)).expect("first register");
        let got = registry.get(id).expect("slot present");
        assert_eq!(got.agent_id, id);
        assert_eq!(got.parent_id, None);
        assert_eq!(got.state, AgentState::Running);
    }

    #[test]
    fn duplicate_register_errors_with_already_registered() {
        let registry = FleetRegistry::new();
        let id = AgentId::generate();
        registry.register(slot(id, None)).unwrap();
        let err = registry.register(slot(id, None)).unwrap_err();
        assert_eq!(err, RegistryError::AlreadyRegistered(id));
    }

    #[test]
    fn unregister_removes_slot() {
        let registry = FleetRegistry::new();
        let id = AgentId::generate();
        registry.register(slot(id, None)).unwrap();
        assert_eq!(registry.len(), 1);
        registry.unregister(id).unwrap();
        assert!(registry.is_empty());
        assert!(registry.get(id).is_none());
    }

    #[test]
    fn unregister_unknown_errors_with_not_found() {
        let registry = FleetRegistry::new();
        let unknown = AgentId::generate();
        let err = registry.unregister(unknown).unwrap_err();
        assert_eq!(err, RegistryError::NotFound(unknown));
    }

    #[test]
    fn set_state_updates_existing_slot() {
        let registry = FleetRegistry::new();
        let id = AgentId::generate();
        registry.register(slot(id, None)).unwrap();
        registry.set_state(id, AgentState::Done).unwrap();
        assert_eq!(registry.get(id).unwrap().state, AgentState::Done);
    }

    #[test]
    fn set_state_unknown_errors_with_not_found() {
        let registry = FleetRegistry::new();
        let unknown = AgentId::generate();
        let err = registry.set_state(unknown, AgentState::Failed).unwrap_err();
        assert_eq!(err, RegistryError::NotFound(unknown));
    }

    #[test]
    fn children_of_zero_children_returns_empty() {
        let registry = FleetRegistry::new();
        let parent = AgentId::generate();
        registry.register(slot(parent, None)).unwrap();
        assert!(registry.children_of(parent).is_empty());
    }

    #[test]
    fn children_of_one_child_returns_singleton() {
        let registry = FleetRegistry::new();
        let parent = AgentId::generate();
        let child = AgentId::generate();
        registry.register(slot(parent, None)).unwrap();
        registry.register(slot(child, Some(parent))).unwrap();
        let kids = registry.children_of(parent);
        assert_eq!(kids, vec![child]);
    }

    #[test]
    fn children_of_many_children_returns_all() {
        let registry = FleetRegistry::new();
        let parent = AgentId::generate();
        registry.register(slot(parent, None)).unwrap();
        let mut expected = Vec::new();
        for _ in 0..5 {
            let child = AgentId::generate();
            registry.register(slot(child, Some(parent))).unwrap();
            expected.push(child);
        }
        // AgentId is not Ord, so sort by hex representation to get
        // a stable order for the comparison.
        let mut got_hex: Vec<String> = registry
            .children_of(parent)
            .into_iter()
            .map(|id| id.to_hex())
            .collect();
        got_hex.sort();
        let mut expected_hex: Vec<String> = expected
            .iter()
            .map(aura_core_types::AgentId::to_hex)
            .collect();
        expected_hex.sort();
        assert_eq!(got_hex, expected_hex);
    }

    #[test]
    fn children_of_unrelated_parent_excludes_unrelated_children() {
        let registry = FleetRegistry::new();
        let parent_a = AgentId::generate();
        let parent_b = AgentId::generate();
        let child_a = AgentId::generate();
        let child_b = AgentId::generate();
        registry.register(slot(parent_a, None)).unwrap();
        registry.register(slot(parent_b, None)).unwrap();
        registry.register(slot(child_a, Some(parent_a))).unwrap();
        registry.register(slot(child_b, Some(parent_b))).unwrap();
        assert_eq!(registry.children_of(parent_a), vec![child_a]);
        assert_eq!(registry.children_of(parent_b), vec![child_b]);
    }
}
