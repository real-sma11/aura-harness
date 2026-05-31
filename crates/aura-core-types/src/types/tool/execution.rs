//! Recorded tool executions and the per-call execution context.

use super::proposal::ToolGateVerdict;
use crate::ids::AgentId;
use serde::{Deserialize, Serialize};

/// Tool execution result from the kernel.
///
/// This records what actually happened after policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecution {
    /// Reference to the original proposal's `tool_use_id`
    pub tool_use_id: String,
    /// Tool name
    pub tool: String,
    /// Tool arguments (copied from proposal for auditability)
    pub args: serde_json::Value,
    /// Kernel's decision
    pub decision: ToolGateVerdict,
    /// Reason for the decision (especially for denials)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Execution result (if approved and executed)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Whether the execution failed (only relevant if approved)
    #[serde(default)]
    pub is_error: bool,
    /// Parent agent that initiated this delegate. Populated on every
    /// cross-agent tool invocation so the record log captures the full
    /// parent chain. Required field — no serde default.
    pub parent_agent_id: AgentId,
    /// Originating end-user id that ultimately triggered this delegate
    /// chain. Preserved along the parent chain for billing attribution
    /// and audit. Required field — no serde default.
    pub originating_user_id: String,
}

/// Context passed alongside tool calls to installed tool endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallContext {
    pub workspace: String,
    pub agent_id: String,
}
