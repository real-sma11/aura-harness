//! `delegate_task` — phase 5 cross-agent tool.
//!
//! Emits a Delegate-tagged task payload to a target agent within the caller's
//! scope. Requires [`Capability::ControlAgent`].
//!
//! Runtime effect (task enqueue + Delegate transaction) flows through
//! [`crate::AgentControlHook::delegate_task`] when wired. The tool records
//! `parent_agent_id` + `originating_user_id` on the outcome so the
//! billing chain remains intact downstream (see
//! `aura_kernel::billing::walk_parent_chain`).

use crate::agents::send_to_agent::{evaluate_control_gate, missing_runtime_hook};
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext};
use async_trait::async_trait;
use aura_core::{Capability, ToolDefinition, ToolResult};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const DELEGATE_TASK_TOOL_NAME: &str = "delegate_task";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateTaskInput {
    pub agent_id: String,
    /// Task prompt / instruction to hand to the child.
    pub task: String,
    /// Optional structured context forwarded with the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegateTaskOutcome {
    pub target_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_id: Option<String>,
    pub dispatched: bool,
}

pub struct DelegateTaskTool;

impl DelegateTaskTool {
    #[must_use]
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            DELEGATE_TASK_TOOL_NAME,
            "Delegate a task to another agent within the caller's scope. \
             Emits a Delegate-tagged task with parent/originating-user \
             attribution intact. Requires Capability::ControlAgent.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "task": { "type": "string" },
                    "context": {}
                },
                "required": ["agent_id", "task"]
            }),
        )
    }

    pub fn evaluate(
        ctx: &ToolContext,
        input: &DelegateTaskInput,
    ) -> Result<DelegateTaskOutcome, ToolError> {
        evaluate_control_gate(ctx, &input.agent_id, "delegate_task")?;
        Ok(DelegateTaskOutcome {
            target_agent_id: input.agent_id.clone(),
            parent_agent_id: ctx.caller_agent_id.map(|id| id.to_string()),
            originating_user_id: ctx.originating_user_id.clone(),
            dispatched: false,
        })
    }
}

#[async_trait]
impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        DELEGATE_TASK_TOOL_NAME
    }

    fn definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn required_capabilities(&self) -> Vec<Capability> {
        vec![Capability::ControlAgent]
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let input: DelegateTaskInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("delegate_task: {e}")))?;

        let mut outcome = match Self::evaluate(ctx, &input) {
            Ok(o) => o,
            Err(err) => {
                return Ok(ToolResult::failure(
                    DELEGATE_TASK_TOOL_NAME,
                    Bytes::from(err.to_string().into_bytes()),
                ));
            }
        };

        let Some(hook) = ctx.agent_control_hook.as_ref() else {
            return Ok(missing_runtime_hook(DELEGATE_TASK_TOOL_NAME));
        };

        let parent = ctx.caller_agent_id.map(|id| id.to_string());
        match hook
            .delegate_task(
                &input.agent_id,
                parent.as_deref(),
                ctx.originating_user_id.as_deref(),
                &input.task,
                input.context.as_ref(),
                ctx.caller_model_id.as_deref(),
            )
            .await
        {
            Ok(()) => outcome.dispatched = true,
            Err(err) => {
                return Ok(ToolResult::failure(
                    DELEGATE_TASK_TOOL_NAME,
                    Bytes::from(format!("delegate_task hook: {err}").into_bytes()),
                ));
            }
        }

        let body = serde_json::to_vec(&outcome)
            .map_err(|e| ToolError::Serialization(format!("delegate_task outcome: {e}")))?;
        Ok(ToolResult::success(DELEGATE_TASK_TOOL_NAME, body)
            .with_metadata("target_agent_id", outcome.target_agent_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Sandbox;
    use crate::ToolConfig;
    use aura_core::{AgentId, AgentPermissions, AgentScope};

    fn ctx(caller: AgentPermissions) -> ToolContext {
        let dir = std::env::temp_dir();
        let mut ctx = ToolContext::new(Sandbox::new(&dir).unwrap(), ToolConfig::default());
        ctx.caller_permissions = Some(caller);
        ctx.caller_agent_id = Some(AgentId::generate());
        ctx.originating_user_id = Some("user-root".into());
        ctx
    }

    #[test]
    fn requires_control_capability() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ReadAgent],
        };
        let input = DelegateTaskInput {
            agent_id: "t".into(),
            task: "do stuff".into(),
            context: None,
        };
        let err = DelegateTaskTool::evaluate(&ctx(caller), &input).unwrap_err();
        assert!(err.to_string().contains("permissions:"), "got: {err}");
        assert!(err.to_string().contains("ControlAgent"), "got: {err}");
    }

    #[test]
    fn denies_out_of_scope_target() {
        let caller = AgentPermissions {
            scope: AgentScope {
                agent_ids: vec!["ok".into()],
                ..AgentScope::default()
            },
            capabilities: vec![Capability::ControlAgent],
        };
        let input = DelegateTaskInput {
            agent_id: "nope".into(),
            task: "do stuff".into(),
            context: None,
        };
        let err = DelegateTaskTool::evaluate(&ctx(caller), &input).unwrap_err();
        assert!(err.to_string().contains("permissions:"), "got: {err}");
    }

    #[test]
    fn allows_valid_delegation_preserves_originator() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let input = DelegateTaskInput {
            agent_id: "any".into(),
            task: "do the needful".into(),
            context: Some(serde_json::json!({"priority": "high"})),
        };
        let outcome = DelegateTaskTool::evaluate(&ctx(caller), &input).unwrap();
        assert_eq!(outcome.target_agent_id, "any");
        assert_eq!(outcome.originating_user_id.as_deref(), Some("user-root"));
        assert!(!outcome.dispatched);
    }
}
