//! Derived [`SubagentSpec`] + supporting attribution / lineage types.

use aura_core::AgentId;
use aura_core_modes::{AgentMode, JoinPolicy, KernelMode, ModeProfile, ReplayMode, SpawnMode};
use aura_core_permissions::Permissions;

use crate::manifest::OverrideManifest;
use crate::overrides::SubagentBudget;

/// Audit attribution stamped onto every derived child. Never
/// overridable by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditAttribution {
    /// Spawning parent agent id.
    pub parent_agent_id: AgentId,
}

/// Parent chain plus the root agent id; used by depth limits,
/// attribution, and `aura agents tree`-style rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentLineage {
    /// Root agent id at the top of the parent chain.
    pub root_agent_id: AgentId,
    /// Parent → root chain in spawn order (inclusive of the parent
    /// agent id, exclusive of the new child).
    pub chain: Vec<AgentId>,
}

impl SubagentLineage {
    /// Convenience constructor for a child whose parent IS the root
    /// agent.
    #[must_use]
    pub fn from_root(root_agent_id: AgentId) -> Self {
        Self {
            root_agent_id,
            chain: vec![root_agent_id],
        }
    }
}

/// Derived subagent spec — the input contract for
/// `aura-fleet-spawn::spawn`.
#[derive(Clone, Debug)]
pub struct SubagentSpec {
    /// Spawning parent agent id.
    pub parent: AgentId,
    /// Child depth from the root (parent depth + 1).
    pub depth: u32,
    /// Child's resolved [`AgentMode`].
    pub mode: AgentMode,
    /// Child's resolved [`ModeProfile`] (mirrors parent's by default;
    /// kernel-mode override updates the `kernel` field).
    pub mode_profile: ModeProfile,
    /// Child's effective [`Permissions`] (intersected from parent +
    /// any override).
    pub permissions: Permissions,
    /// Child's [`KernelMode`] (mirrors `mode_profile.kernel`).
    pub kernel_mode: KernelMode,
    /// Child's selected model id.
    pub model_id: String,
    /// Free-form child role tag — `"task"`, `"reviewer"`, ...
    pub kind: String,
    /// Child's [`SpawnMode`].
    pub spawn_mode: SpawnMode,
    /// Child's join policy (only meaningful for batch spawns).
    pub join_policy: JoinPolicy,
    /// Child's [`ReplayMode`].
    pub replay_mode: ReplayMode,
    /// Resolved budget.
    pub budget: SubagentBudget,
    /// Optional tool subset.
    pub tool_subset: Option<Vec<String>>,
    /// Optional isolation environment id.
    pub isolation_id: Option<String>,
    /// Parent → root lineage extended with the parent agent id.
    pub lineage: SubagentLineage,
    /// Stamped attribution; never overridable.
    pub audit_attribution: AuditAttribution,
    /// Manifest of fields the caller explicitly overrode.
    pub overridden_fields: OverrideManifest,
}
