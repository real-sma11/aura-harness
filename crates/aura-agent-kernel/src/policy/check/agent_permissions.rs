//! Per-agent tool permission resolution.
//!
//! Two responsibilities:
//! 1. [`Policy::resolve_tool_state`] — collapse the two-level user / agent
//!    permission model into a single [`ToolState`] tri-state per tool.
//! 2. [`Policy::check_agent_permissions`] — verify a [`ToolCall`] satisfies
//!    the caller's `AgentPermissions` (capability + scope).

use super::scope::scope_violation;
use super::verdict::{PolicyResult, PolicyVerdict};
use super::Policy;
use aura_core::{resolve_effective_permission, ToolCall, ToolState};

impl Policy {
    /// Resolve the tri-state `on` / `off` / `ask` [`ToolState`] for
    /// `tool` against the two-level permission model (user default +
    /// per-agent override). This is the single resolution helper the
    /// kernel gate consults for per-tool enablement.
    #[must_use]
    pub fn resolve_tool_state(&self, tool: &str) -> ToolState {
        resolve_effective_permission(
            &self.config.user_default,
            self.config.agent_override.as_ref(),
            tool,
        )
    }

    /// Evaluate a [`ToolCall`] against the caller's `AgentPermissions`.
    /// Returns `None` when the call passes, or `Some(rejection)` when the
    /// call is denied because the caller lacks the required capability or
    /// the args target an out-of-scope org / project / agent.
    ///
    /// Always on — there is no feature flag or opt-out.
    pub(super) fn check_agent_permissions(&self, tool_call: &ToolCall) -> Option<PolicyResult> {
        let permissions = &self.config.agent_permissions;

        if let Some(required) = self
            .config
            .tool_capability_requirements
            .get(&tool_call.tool)
        {
            // Route through `Capability::satisfies` so project wildcards
            // (`ReadAllProjects` / `WriteAllProjects`) on the bundle cover
            // an exact-id `ReadProject { id }` / `WriteProject { id }`
            // tool requirement. Keeps harness kernel enforcement aligned
            // with `aura-os-agent-runtime::policy::holds_capability`.
            let held = permissions
                .capabilities
                .iter()
                .any(|held| held.satisfies(required));
            if !held {
                return Some(
                    PolicyVerdict::Deny {
                        reason: format!("permissions: requires capability {required:?}"),
                    }
                    .into(),
                );
            }
        }

        if let Some(reason) = scope_violation(&permissions.scope, &tool_call.args) {
            return Some(PolicyVerdict::Deny { reason }.into());
        }

        None
    }
}
