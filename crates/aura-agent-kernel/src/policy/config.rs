//! Policy configuration: [`PolicyConfig`] and related policy shape types.
//!
//! All behavior-free, data-shape pieces of the policy engine live here so
//! [`super::check`] can focus on the authorization pipeline itself.

use aura_core::{
    ActionKind, AgentPermissions, AgentToolPermissions, Capability, InstalledIntegrationDefinition,
    InstalledToolIntegrationRequirement, UserToolDefaults,
};
use std::collections::{HashMap, HashSet};

// ============================================================================
// Policy Configuration
// ============================================================================

/// Policy configuration.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    /// Allowed action kinds
    pub allowed_action_kinds: HashSet<ActionKind>,
    /// Maximum proposals per request. Exposed via [`super::Policy::max_proposals`]; the kernel truncates proposals exceeding this limit.
    pub max_proposals: usize,
    /// Installed integrations currently authorized for this runtime.
    pub installed_integrations: Vec<InstalledIntegrationDefinition>,
    /// Declared integration requirements for tools.
    pub tool_integration_requirements: HashMap<String, InstalledToolIntegrationRequirement>,
    /// Scope + capability bundle for the agent this policy governs.
    /// Always consulted on `Delegate` proposals â€” the check is
    /// unconditional and cannot be disabled. [`AgentPermissions::full_access`]
    /// grants everything; callers that need a locked-down agent must pass an
    /// explicit narrower bundle.
    pub agent_permissions: AgentPermissions,
    /// Mapping from tool name to the [`Capability`] required to use it.
    /// Tools not listed here carry no capability requirement.
    pub tool_capability_requirements: HashMap<String, Capability>,
    /// User-level tool permission defaults — the originating user's
    /// "default permissions" / "auto-review" / "full access" mode. Every
    /// `(agent, tool)` resolves through this plus the optional
    /// [`Self::agent_override`] via
    /// [`aura_core::resolve_effective_permission`]. Defaults to
    /// [`UserToolDefaults::full_access`] to preserve "default ON" for
    /// callers that do not load a persisted user profile.
    ///
    pub user_default: UserToolDefaults,
    /// Optional per-agent override map stamped on this agent's
    /// [`aura_core::Identity`]. `None` (or an empty map) means
    /// "inherit the user default verbatim". Populated entries override
    /// only that specific tool; anything unlisted still flows through
    /// [`Self::user_default`].
    pub agent_override: Option<AgentToolPermissions>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let mut allowed_action_kinds = HashSet::new();
        allowed_action_kinds.insert(ActionKind::Reason);
        allowed_action_kinds.insert(ActionKind::Memorize);
        allowed_action_kinds.insert(ActionKind::Decide);
        allowed_action_kinds.insert(ActionKind::Delegate);

        let mut tool_capability_requirements = HashMap::new();
        tool_capability_requirements.insert("spawn_agent".to_string(), Capability::SpawnAgent);
        tool_capability_requirements.insert("task".to_string(), Capability::SpawnAgent);
        tool_capability_requirements.insert("send_to_agent".to_string(), Capability::ControlAgent);
        tool_capability_requirements
            .insert("agent_lifecycle".to_string(), Capability::ControlAgent);
        tool_capability_requirements.insert("delegate_task".to_string(), Capability::ControlAgent);
        tool_capability_requirements.insert("get_agent_state".to_string(), Capability::ReadAgent);
        tool_capability_requirements.insert("list_agents".to_string(), Capability::ListAgents);
        tool_capability_requirements.insert("run_command".to_string(), Capability::InvokeProcess);
        tool_capability_requirements.insert("post_to_feed".to_string(), Capability::PostToFeed);
        tool_capability_requirements.insert("check_budget".to_string(), Capability::ManageBilling);
        tool_capability_requirements.insert("record_usage".to_string(), Capability::ManageBilling);

        Self {
            allowed_action_kinds,
            max_proposals: 8,
            installed_integrations: Vec::new(),
            tool_integration_requirements: HashMap::new(),
            agent_permissions: AgentPermissions::full_access(),
            tool_capability_requirements,
            user_default: UserToolDefaults::full_access(),
            agent_override: None,
        }
    }
}

impl PolicyConfig {
    /// Replace the installed integrations set for this runtime.
    pub fn set_installed_integrations(
        &mut self,
        integrations: impl IntoIterator<Item = InstalledIntegrationDefinition>,
    ) {
        self.installed_integrations = integrations.into_iter().collect();
    }

    /// Replace the tool-to-integration requirement map for this runtime.
    pub fn set_tool_integration_requirements(
        &mut self,
        requirements: impl IntoIterator<Item = (String, InstalledToolIntegrationRequirement)>,
    ) {
        self.tool_integration_requirements = requirements.into_iter().collect();
    }

    /// Attach an [`AgentPermissions`] bundle to this policy.
    #[must_use]
    pub fn with_agent_permissions(mut self, permissions: AgentPermissions) -> Self {
        self.agent_permissions = permissions;
        self
    }

    /// Declare the [`Capability`] required to invoke `tool`.
    #[must_use]
    pub fn with_tool_capability(mut self, tool: impl Into<String>, cap: Capability) -> Self {
        self.tool_capability_requirements.insert(tool.into(), cap);
        self
    }

    /// Attach the originating user's tool-permission defaults. This is
    /// the first half of the tri-state (`on`/`off`/`ask`) resolution
    /// consulted by [`aura_core::resolve_effective_permission`].
    #[must_use]
    pub fn with_user_default(mut self, user_default: UserToolDefaults) -> Self {
        self.user_default = user_default;
        self
    }

    /// Attach the per-agent override map. `None` / empty map means
    /// "inherit the user default verbatim". Populated entries override
    /// only those specific tools; anything unlisted still flows through
    /// [`Self::user_default`].
    #[must_use]
    pub fn with_agent_override(mut self, agent_override: Option<AgentToolPermissions>) -> Self {
        self.agent_override = agent_override;
        self
    }
}
