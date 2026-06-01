//! Shared subagent request/result data.
//!
//! These are behavior-free wire/data shapes. Runtime orchestration belongs in
//! `aura-runtime`; tool dispatch belongs in `aura-tools`.

use crate::{AgentId, AgentPermissions, AgentToolPermissions, Capability, UserToolDefaults};
use serde::{Deserialize, Serialize};

/// Default maximum wall-clock time for a foreground subagent run.
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 300_000;

/// Canonical cap on agentic steps.
///
/// Single source of truth for every "max turns / max iterations" knob in
/// the system:
///
/// - [`SubagentBudget::default`]'s `max_iterations`,
/// - `aura_runtime::session::state::Session::max_turns` default and the
///   wire-level `RuntimeRequest.model.max_turns` override,
/// - `aura_agent::AgentLoopConfig::{max_iterations, max_turns_per_task,
///   max_iterations_per_task}` defaults,
/// - `aura_agent::AgentRunnerConfig::max_agentic_iterations` default,
/// - the integration-test harness default in `tests/common`.
///
/// Layers that consume a `usize` (e.g. `AgentLoopConfig::max_iterations`)
/// cast `MAX_TURNS as usize` at the call site so this constant remains
/// the only place the numeric value lives. Callers wanting a different
/// bound — typically tests asserting budget-exhaustion behavior, or
/// clients sending an explicit `RuntimeRequest.model.max_turns` — still pass an
/// override locally; the constant only governs the default.
pub const MAX_TURNS: u32 = 300;

/// Runtime limits for a subagent kind or concrete dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentBudget {
    /// Maximum child agent-loop iterations.
    pub max_iterations: u32,
    /// Optional response token cap for the child loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Wall-clock timeout for the foreground task.
    pub timeout_ms: u64,
}

impl Default for SubagentBudget {
    fn default() -> Self {
        Self {
            max_iterations: MAX_TURNS,
            max_tokens: None,
            timeout_ms: DEFAULT_SUBAGENT_TIMEOUT_MS,
        }
    }
}

/// A bundled subagent kind exposed by the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentKindSpec {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    /// Tool names this kind may see/use after parent permissions are applied.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Capabilities this kind may retain from its parent.
    #[serde(default)]
    pub allowed_capabilities: Vec<Capability>,
    /// Readonly kinds must also be enforced by executor guardrails.
    #[serde(default)]
    pub readonly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default)]
    pub budget: SubagentBudget,
}

/// Request handed from the `task` tool to runtime dispatch.
///
/// Phase 7b additively extends the request with the parent's
/// resolved `AgentMode` / `KernelMode` / `model_id` snapshot so the
/// fleet adapter no longer synthesises placeholder values. The new
/// fields are all `#[serde(default)]` so a Phase 7a-shaped JSON body
/// continues to deserialise without change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentDispatchRequest {
    pub parent_agent_id: AgentId,
    pub subagent_type: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_id: Option<String>,
    #[serde(default)]
    pub parent_chain: Vec<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_addendum: Option<String>,
    pub parent_permissions: AgentPermissions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_permissions: Option<AgentToolPermissions>,
    pub user_tool_defaults: UserToolDefaults,
    /// Phase 7b: caller-stamped tool-call id used to dedupe
    /// idempotent re-dispatches. `None` opts the spawn out of
    /// dedupe (the legacy Phase 7a shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Phase 7b: explicit parent `AgentMode` snapshot. `None`
    /// inherits the Phase-7a-default `Agent` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_mode: Option<crate::AgentMode>,
    /// Phase 7b: explicit parent `KernelMode` snapshot. `None`
    /// inherits the Phase-7a-default `Audited` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kernel_mode: Option<crate::KernelMode>,
    /// Phase 7b: explicit parent model identifier snapshot. Empty
    /// string preserves the Phase-7a placeholder behaviour for
    /// callers that did not yet thread the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_model_id: Option<String>,
    /// Phase 7b: caller-specified `AgentMode` override for the
    /// child. Must narrow the parent's effective mode. `None`
    /// inherits the parent mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_mode: Option<crate::AgentMode>,
    /// Phase 7b: caller-specified `Permissions` override for the
    /// child. Must be a subset of the parent's. `None` inherits
    /// the parent permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_permissions: Option<AgentPermissions>,
    /// Phase 7b: caller-specified explicit `tool_subset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_tool_subset: Option<Vec<String>>,
    /// Phase 7b: caller-specified isolation environment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_isolation_id: Option<String>,
    /// Phase 7b: caller-specified budget override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_budget: Option<SubagentBudget>,
    /// Spawn mode for this dispatch. `None` (the legacy shape) means
    /// the dispatcher uses its default (`SpawnMode::Wait`): the parent
    /// turn blocks until the child completes and reads the result
    /// inline. `Some(Detached)` opts into background spawning so the
    /// parent can continue and observe the child's live thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_mode: Option<crate::SpawnMode>,
    /// AURA Council slot index for a council-member dispatch. `Some(i)`
    /// marks this dispatch as council member `i` so the runtime
    /// observability hook stamps `council_index` (and the member model)
    /// onto the emitted `SubagentSpawned`. `None` (the default) for every
    /// ordinary `task` spawn, which keeps those spawns emitting
    /// `council_index: None` exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub council_index: Option<u32>,
}

/// Terminal state of a foreground subagent task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentExit {
    Completed,
    Failed { reason: String },
    Cancelled,
    Timeout,
    Rejected { reason: String },
}

/// Result returned to the parent `task` tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_agent_id: Option<AgentId>,
    pub final_message: String,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub files_changed: Vec<String>,
    pub exit: SubagentExit,
}

impl SubagentResult {
    #[must_use]
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            child_agent_id: None,
            final_message: String::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit: SubagentExit::Rejected {
                reason: reason.into(),
            },
        }
    }

    #[must_use]
    pub fn completed(child_agent_id: AgentId, final_message: impl Into<String>) -> Self {
        Self {
            child_agent_id: Some(child_agent_id),
            final_message: final_message.into(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit: SubagentExit::Completed,
        }
    }
}
