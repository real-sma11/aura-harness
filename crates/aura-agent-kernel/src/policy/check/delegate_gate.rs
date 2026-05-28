//! [`ActionKind::Delegate`] proposal parsing and evaluation.
//!
//! A `Delegate` proposal carries a serialized [`ToolCall`] in its
//! payload. The delegate gate parses the payload, runs the per-agent
//! permission check, then defers to the tool / runtime-capability
//! checks for the final verdict.

use super::verdict::PolicyVerdict;
use super::Policy;
use aura_core::{RuntimeCapabilityInstall, ToolCall};
use tracing::warn;

impl Policy {
    /// Evaluate a `Delegate` proposal payload against the agent
    /// permission model and the tool / runtime-capability gate.
    ///
    /// Returns the resulting [`PolicyVerdict`]. `Allow` flows back into
    /// the parent orchestrator so subsequent gates (e.g. action-kind)
    /// remain in charge of the final answer.
    pub(super) fn evaluate_delegate(
        &self,
        payload: &[u8],
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
    ) -> PolicyVerdict {
        match serde_json::from_slice::<ToolCall>(payload) {
            Ok(tool_call) => {
                if let Some(result) = self.check_agent_permissions(&tool_call) {
                    if !result.allowed {
                        return PolicyVerdict::Deny {
                            reason: result.reason.unwrap_or_else(|| "Policy denied".to_string()),
                        };
                    }
                }

                self.check_tool_with_runtime_capabilities_verdict(
                    &tool_call.tool,
                    &tool_call.args,
                    runtime_capabilities,
                )
            }
            Err(_) => {
                warn!("Malformed delegate payload");
                PolicyVerdict::Deny {
                    reason: "Malformed delegate payload".to_string(),
                }
            }
        }
    }
}
