//! Caller-supplied overrides + per-child budget.

use aura_core_modes::{AgentMode, JoinPolicy, KernelMode, ReplayMode, SpawnMode};
use aura_core_permissions::Permissions;
use aura_core_types::{AgentToolPermissions, UserToolDefaults};

/// Per-spawn budget passed through to the agent loop / fleet.
///
/// Phase 6a treats the budget as a flat triple of caps; the wire
/// shape and richer surface (per-tool token caps, hierarchical
/// budgets) land in Phase 7+.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentBudget {
    /// Hard cap on total tokens the child may consume.
    pub max_tokens: u32,
    /// Hard cap on agent-loop iterations the child may run.
    pub max_iterations: u32,
    /// Wall-clock timeout in milliseconds.
    pub timeout_ms: u64,
}

impl SubagentBudget {
    /// Default budget used when the spawn omits an explicit override:
    /// 64K tokens, 50 iterations, 5-minute timeout.
    #[must_use]
    pub fn default_for_phase_6a() -> Self {
        Self {
            max_tokens: 64_000,
            max_iterations: 50,
            timeout_ms: 300_000,
        }
    }
}

/// Caller-supplied subagent overrides. Every field is `Option<T>`;
/// `None` means "inherit", `Some(_)` means "explicit override —
/// validate against the narrowing-only rule".
#[derive(Clone, Debug, Default)]
pub struct SubagentOverrides {
    /// Override the child's [`AgentMode`].
    pub mode: Option<AgentMode>,
    /// Override the child's [`Permissions`] (intersection-only).
    pub permissions: Option<Permissions>,
    /// Override the child's [`KernelMode`].
    pub kernel_mode: Option<KernelMode>,
    /// Override the child's model id.
    pub model_id: Option<String>,
    /// Override the child's kind tag (a free-form role label —
    /// `"task"`, `"reviewer"`, `"explorer"`, etc.).
    pub kind: Option<String>,
    /// Override the child's [`SpawnMode`].
    pub spawn_mode: Option<SpawnMode>,
    /// Override the join policy applied to a batch spawn.
    pub join_policy: Option<JoinPolicy>,
    /// Override the child's [`ReplayMode`]. Defaults to [`ReplayMode::Live`].
    pub replay_mode: Option<ReplayMode>,
    /// Override the child's budget.
    pub budget: Option<SubagentBudget>,
    /// Restrict the child to a subset of tools.
    pub tool_subset: Option<Vec<String>>,
    /// Pin the child to a specific isolation environment id.
    pub isolation_id: Option<String>,
    /// Phase 7b: bundled subagent kind id (`"explore"`,
    /// `"general_purpose"`, ...). Distinct from [`Self::kind`]
    /// (which is a free-form role tag); `subagent_type` keys the
    /// kind registry the runtime uses to look up the system prompt,
    /// capability allowlist, and default model. When the dispatcher
    /// supplies an explicit `subagent_type` it also takes precedence
    /// over [`Self::kind`] for stamping the
    /// `OverrideManifest::SubagentType` entry.
    pub subagent_type: Option<String>,
    /// Phase 7b: free-form addendum appended to the kind's
    /// system prompt before the child runs.
    pub system_prompt_addendum: Option<String>,
    /// Phase 7b: parent's per-tool override map (used by the
    /// legacy per-tool policy resolver in the child runner).
    pub parent_tool_permissions: Option<AgentToolPermissions>,
    /// Phase 7b: user-level default tool policy applied to the
    /// child. `None` inherits the runtime's full-access defaults.
    pub user_tool_defaults: Option<UserToolDefaults>,
}
