//! Policy engine proper — the [`Policy`] type and its authorization
//! pipeline.
//!
//! `check` holds everything that actively *evaluates* a [`Proposal`] or
//! [`ToolCall`] against the declarative [`super::config::PolicyConfig`].
//! The pipeline is split across siblings:
//!
//! - [`verdict`] — `PolicyVerdict` + `PolicyResult` shim types.
//! - [`agent_permissions`] — `Policy::resolve_tool_state` and the
//!   capability + scope check on a `ToolCall`.
//! - [`integration_gate`] — the integration-requirement check that
//!   delegates to `aura_core::installed_integrations_satisfy`.
//! - [`scope`] — `target_*` scope-key validation.
//! - [`delegate_gate`] — `ActionKind::Delegate` payload parsing and
//!   evaluation against the per-agent and tool gates.

mod agent_permissions;
mod delegate_gate;
mod integration_gate;
mod scope;
#[cfg(test)]
mod tests;
mod verdict;

pub use verdict::{PolicyResult, PolicyVerdict};

use super::config::PolicyConfig;
use crate::{PendingToolPrompt, ToolApprovalRemember};
use aura_core::{ActionKind, Proposal, RuntimeCapabilityInstall, ToolState};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, warn};

// ============================================================================
// Policy Engine
// ============================================================================

/// Policy engine for authorizing proposals and tool usage.
///
/// Uses `std::sync::Mutex` for `session_approvals` intentionally: all
/// accesses are brief `insert`/`contains`/`remove`/`clear` with no
/// `.await` held across the lock, so a sync mutex avoids the overhead
/// of `tokio::sync::Mutex` and the `Send` bound it would impose on
/// callers.
#[derive(Debug)]
pub struct Policy {
    pub(super) config: PolicyConfig,
    session_tool_states: Mutex<HashMap<String, ToolState>>,
}

impl Policy {
    /// Create a new policy with the given config.
    #[must_use]
    pub fn new(config: PolicyConfig) -> Self {
        Self {
            config,
            session_tool_states: Mutex::new(HashMap::new()),
        }
    }

    /// Create a policy with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(PolicyConfig::default())
    }

    /// Cache a live approval decision for this policy's current session.
    pub fn remember_tool_state_for_session(&self, tool: &str, state: ToolState) {
        self.session_tool_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tool.to_string(), state);
    }

    /// Clear all session approvals.
    ///
    /// Recovers gracefully from mutex poisoning by accessing the inner data.
    pub fn clear_session_approvals(&self) {
        self.session_tool_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Return the live prompt verdict for the additive tri-state `ask`
    /// layer. `None` means the new layer has no opinion and the legacy
    /// verdict remains authoritative for Phase C.
    #[must_use]
    pub fn live_tool_prompt_verdict(
        &self,
        tool: &str,
        args: &serde_json::Value,
        agent_id: aura_core::AgentId,
        request_id: String,
        has_live_session: bool,
        remember_options: Vec<ToolApprovalRemember>,
    ) -> Option<PolicyVerdict> {
        if let Some(state) = self
            .session_tool_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tool)
            .copied()
        {
            return match state {
                ToolState::Allow => None,
                ToolState::Deny => Some(PolicyVerdict::Deny {
                    reason: format!("Tool '{tool}' was denied for this session"),
                }),
                ToolState::Ask => None,
            };
        }

        if self.resolve_tool_state(tool) != ToolState::Ask {
            return None;
        }

        if !has_live_session {
            return Some(PolicyVerdict::Deny {
                reason: format!("tool {tool} is set to ask; no session to prompt"),
            });
        }

        Some(PolicyVerdict::RequireApproval {
            reason: format!("Tool '{tool}' is set to ask"),
            prompt: Some(PendingToolPrompt {
                request_id,
                tool_name: tool.to_string(),
                args: args.clone(),
                agent_id,
                remember_options,
            }),
        })
    }

    /// Check if a proposal is allowed.
    #[must_use]
    pub fn check(&self, proposal: &Proposal) -> PolicyResult {
        self.check_with_runtime_capabilities(proposal, None)
    }

    /// Check if a proposal is allowed against an optional persisted runtime
    /// capability snapshot.
    #[must_use]
    pub fn check_with_runtime_capabilities(
        &self,
        proposal: &Proposal,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> PolicyResult {
        self.check_with_runtime_capabilities_verdict(proposal, runtime_capabilities)
            .into()
    }

    /// [`Self::check_with_runtime_capabilities`] returning the richer
    /// [`PolicyVerdict`]. Prefer this in new code so
    /// "needs operator approval" is distinguishable from "hard deny".
    #[must_use]
    pub fn check_with_runtime_capabilities_verdict(
        &self,
        proposal: &Proposal,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> PolicyVerdict {
        if !self
            .config
            .allowed_action_kinds
            .contains(&proposal.action_kind)
        {
            warn!(kind = ?proposal.action_kind, "Action kind not allowed");
            return PolicyVerdict::Deny {
                reason: format!("Action kind {:?} not allowed", proposal.action_kind),
            };
        }

        if proposal.action_kind == ActionKind::Delegate {
            let verdict = self.evaluate_delegate(&proposal.payload, runtime_capabilities);
            if !verdict.is_allowed() {
                return verdict;
            }
        }

        debug!(kind = ?proposal.action_kind, "Proposal allowed");
        PolicyVerdict::Allow
    }

    /// Check if a tool call is allowed (includes session approval check).
    #[must_use]
    pub fn check_tool(&self, tool: &str, _input: &serde_json::Value) -> PolicyResult {
        self.check_tool_with_runtime_capabilities(tool, _input, None)
    }

    /// Check if a tool call is allowed against an optional persisted runtime
    /// capability snapshot.
    #[must_use]
    pub fn check_tool_with_runtime_capabilities(
        &self,
        tool: &str,
        input: &serde_json::Value,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> PolicyResult {
        self.check_tool_with_runtime_capabilities_verdict(tool, input, runtime_capabilities)
            .into()
    }

    /// [`Self::check_tool_with_runtime_capabilities`] returning the
    /// structured [`PolicyVerdict`] instead of the compat shim.
    #[must_use]
    pub fn check_tool_with_runtime_capabilities_verdict(
        &self,
        tool: &str,
        _input: &serde_json::Value,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> PolicyVerdict {
        if let Some(reason) = self.integration_requirement_satisfied(tool, runtime_capabilities) {
            return PolicyVerdict::Deny { reason };
        }

        if let Some(state) = self
            .session_tool_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tool)
            .copied()
        {
            return match state {
                ToolState::Allow => PolicyVerdict::Allow,
                ToolState::Deny => PolicyVerdict::Deny {
                    reason: format!("Tool '{tool}' was denied for this session"),
                },
                ToolState::Ask => PolicyVerdict::RequireApproval {
                    reason: format!("Tool '{tool}' is set to ask"),
                    prompt: None,
                },
            };
        }

        match self.resolve_tool_state(tool) {
            ToolState::Deny => PolicyVerdict::Deny {
                reason: format!("Tool '{tool}' is not allowed"),
            },
            ToolState::Allow => PolicyVerdict::Allow,
            ToolState::Ask => PolicyVerdict::RequireApproval {
                reason: format!("Tool '{tool}' is set to ask"),
                prompt: None,
            },
        }
    }

    /// Get maximum allowed proposals.
    #[must_use]
    pub const fn max_proposals(&self) -> usize {
        self.config.max_proposals
    }
}
