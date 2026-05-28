//! Integration-requirement gate: ensure a tool's `required_integration`
//! is satisfied by an installed integration in the runtime capability
//! ledger or, in fallback mode, the policy config itself.

use super::Policy;
use aura_core::{installed_integrations_satisfy, RuntimeCapabilityInstall};

impl Policy {
    /// Returns `Some(reason)` if `tool` requires an integration that is
    /// not installed in the (preferred) runtime-capability ledger or, as
    /// a fallback, the [`crate::policy::PolicyConfig::installed_integrations`]
    /// list. Returns `None` when the requirement is satisfied or there
    /// is no requirement at all.
    pub(super) fn integration_requirement_satisfied(
        &self,
        tool: &str,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> Option<String> {
        let required_integration = if let Some(runtime_capabilities) = runtime_capabilities {
            match runtime_capabilities.tool_capability(tool) {
                Some(tool_capability) => tool_capability.required_integration.as_ref(),
                None if self.config.tool_integration_requirements.contains_key(tool) => {
                    return Some(format!(
                        "Tool '{tool}' is not installed in the kernel runtime capability ledger"
                    ));
                }
                None => self.config.tool_integration_requirements.get(tool),
            }
        } else {
            self.config.tool_integration_requirements.get(tool)
        };
        let required_integration = required_integration?;

        let installed = runtime_capabilities.map_or_else(
            || {
                installed_integrations_satisfy(
                    required_integration,
                    &self.config.installed_integrations,
                )
            },
            |runtime_capabilities| {
                runtime_capabilities.integration_requirement_satisfied(required_integration)
            },
        );

        if installed {
            None
        } else {
            Some(format!(
                "Tool '{tool}' requires an installed integration{}{}{}",
                required_integration
                    .provider
                    .as_deref()
                    .map(|provider| format!(" with provider '{provider}'"))
                    .unwrap_or_default(),
                required_integration
                    .kind
                    .as_deref()
                    .map(|kind| format!(" and kind '{kind}'"))
                    .unwrap_or_default(),
                required_integration
                    .integration_id
                    .as_deref()
                    .map(|id| format!(" (integration_id '{id}')"))
                    .unwrap_or_default(),
            ))
        }
    }
}
