//! # aura-core-permissions
//!
//! Layer: core
//!
//! Privilege types and pure resolution math: capability enum,
//! agent/tool permission shapes, plugin grant sources, and parent–child
//! narrowing. No I/O, no config reads.
//!
//! ## Invariants
//!
//! - [`narrow`] is the parent→child intersection: the result is always
//!   a subset of both inputs (`narrow(p, c) ⊆ p ∩ c`).
//! - [`intersect`] is commutative and associative.
//! - `effective = AgentMode::default_capability_profile() ∩ user_grants`
//!   — mode can only NARROW, never widen. Computed by
//!   [`effective`].
//!
//! ## Assumptions
//!
//! - Inputs are treated as sets of capability discriminants for the
//!   intersect/narrow math; semantic equality (e.g.
//!   `WriteProject { id }` covering an exact id) is decided by
//!   [`Capability::satisfies`] when the caller uses
//!   [`allows`]/[`allows_tool`].
//!
//! ## Failure modes
//!
//! - [`PermissionError`] — emitted by the resolver when no grant
//!   satisfies a required capability.
//!
//! ## Cross-crate layering
//!
//! Depends on `aura-core-modes` for [`CapabilityProfile`]. No other
//! `aura-*` deps.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod capability;
mod effective;
mod grants;
mod math;
mod scope;

pub use capability::Capability;
pub use effective::{effective, EffectivePermissions, PermissionDecision, PermissionError};
pub use grants::{GrantSource, PrivilegeGrant};
pub use math::{allows, allows_tool, intersect, narrow};
pub use scope::AgentScope;

use serde::{Deserialize, Serialize};

/// A bundle of scope + capabilities attached to an agent record.
///
/// This is the canonical permission type. [`AgentPermissions`] is
/// retained as a type alias for source compatibility with pre-split
/// call sites (`aura_core_types::AgentPermissions`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    /// Scope (orgs/projects/agent ids).
    #[serde(default)]
    pub scope: AgentScope,
    /// Capability grants.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

/// Legacy alias for [`Permissions`].
pub type AgentPermissions = Permissions;

impl Permissions {
    /// Fully permissive bundle: universe scope + every capability
    /// variant that does not require a host-specific id.
    #[must_use]
    pub fn full_access() -> Self {
        Self {
            scope: AgentScope::default(),
            capabilities: vec![
                Capability::SpawnAgent,
                Capability::ControlAgent,
                Capability::ReadAgent,
                Capability::ListAgents,
                Capability::ManageOrgMembers,
                Capability::ManageBilling,
                Capability::InvokeProcess,
                Capability::PostToFeed,
                Capability::GenerateMedia,
                Capability::ComputerUse,
                Capability::ReadAllProjects,
                Capability::WriteAllProjects,
            ],
        }
    }

    /// Historical alias for the bootstrap super-agent bundle (same as
    /// [`Self::full_access`]).
    #[must_use]
    pub fn ceo_preset() -> Self {
        Self::full_access()
    }

    /// Legacy default applied by the phase 6 migrator; identical to
    /// [`Self::full_access`].
    #[must_use]
    pub fn legacy_default() -> Self {
        Self::full_access()
    }

    /// Empty permissions: universe scope (vacuously), zero
    /// capabilities. Strict subset of every other `Permissions`.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// True iff every capability in `other` is satisfied by `self`
    /// **and** `other.scope` is contained in `self.scope`.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        if !self.scope.contains(&other.scope) {
            return false;
        }
        other
            .capabilities
            .iter()
            .all(|req| self.capabilities.iter().any(|held| held.satisfies(req)))
    }
}

/// Per-tool override map. `None` (or empty map) means "inherit the
/// user default for every tool".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToolPermissions {
    /// Per-tool tri-state overrides.
    #[serde(default)]
    pub per_tool: std::collections::BTreeMap<String, ToolState>,
}

impl AgentToolPermissions {
    /// Empty override map (inherit user default).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style insert.
    #[must_use]
    pub fn with(mut self, tool: impl Into<String>, state: ToolState) -> Self {
        self.per_tool.insert(tool.into(), state);
        self
    }

    /// True iff the override map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.per_tool.is_empty()
    }
}

/// Resolved permission for an `(agent, tool)` pair. Tri-state:
/// `on` / `off` / `ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolState {
    /// `"on"` — execute without prompting.
    #[serde(rename = "on", alias = "allow")]
    Allow,
    /// `"off"` — reject the call.
    #[serde(rename = "off", alias = "deny")]
    Deny,
    /// `"ask"` — suspend and prompt the user.
    #[serde(rename = "ask")]
    Ask,
}

impl ToolState {
    /// Monotonic permission ordering used for per-agent overrides:
    /// `off < ask < on`.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Deny => 0,
            Self::Ask => 1,
            Self::Allow => 2,
        }
    }

    /// True iff `self` is no broader than `parent` under
    /// `off < ask < on`.
    #[must_use]
    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.rank() <= parent.rank()
    }
}

/// User-scoped default permissions applied to every agent owned by
/// that user (subject to per-agent overrides).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserToolDefaults {
    /// One of the three default modes.
    pub mode: UserDefaultMode,
}

impl UserToolDefaults {
    /// `"on"` for every tool.
    #[must_use]
    pub fn full_access() -> Self {
        Self {
            mode: UserDefaultMode::FullAccess,
        }
    }

    /// `"ask"` for every tool.
    #[must_use]
    pub fn auto_review() -> Self {
        Self {
            mode: UserDefaultMode::AutoReview,
        }
    }

    /// Custom per-tool map with a `fallback` for tools not in the map.
    #[must_use]
    pub fn default_permissions(
        per_tool: std::collections::BTreeMap<String, ToolState>,
        fallback: ToolState,
    ) -> Self {
        Self {
            mode: UserDefaultMode::DefaultPermissions { per_tool, fallback },
        }
    }
}

impl Default for UserToolDefaults {
    fn default() -> Self {
        Self::full_access()
    }
}

/// The three client-facing default modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserDefaultMode {
    /// Every tool resolves to `on`.
    FullAccess,
    /// Every tool resolves to `ask`.
    AutoReview,
    /// User-defined map + fallback.
    DefaultPermissions {
        /// Per-tool overrides.
        per_tool: std::collections::BTreeMap<String, ToolState>,
        /// Default for tools not present in `per_tool`.
        fallback: ToolState,
    },
}

/// Resolve the effective [`ToolState`] for `tool`.
#[must_use]
pub fn resolve_effective_permission(
    user_default: &UserToolDefaults,
    agent_override: Option<&AgentToolPermissions>,
    tool: &str,
) -> ToolState {
    if let Some(state) = agent_override.and_then(|o| o.per_tool.get(tool)) {
        return *state;
    }
    match &user_default.mode {
        UserDefaultMode::FullAccess => ToolState::Allow,
        UserDefaultMode::AutoReview => ToolState::Ask,
        UserDefaultMode::DefaultPermissions { per_tool, fallback } => {
            per_tool.get(tool).copied().unwrap_or(*fallback)
        }
    }
}

/// Return whether the current `(user, agent)` tool policy is
/// globally full-access.
#[must_use]
pub fn is_effectively_full_access(
    user_default: &UserToolDefaults,
    agent_override: Option<&AgentToolPermissions>,
) -> bool {
    matches!(user_default.mode, UserDefaultMode::FullAccess)
        && agent_override.map_or(true, |override_permissions| {
            override_permissions
                .per_tool
                .values()
                .all(|state| *state == ToolState::Allow)
        })
}

#[cfg(test)]
mod tests;
