//! `SpawnHook` trait + `NoopSpawnHook` default impl.
//!
//! The `spawn_agent` tool in `aura-tools` produces a permission-checked
//! [`ChildAgentSpec`] and delegates the actual persistence (creating the
//! child `Identity`, seeding its record log, and emitting the `Delegate`
//! transaction on the *caller's* record log) to a `SpawnHook`.
//!
//! The trait lives in this exec-layer crate so the exec-layer `aura-tools`
//! can consume it without an upward dependency on the agent-layer kernel.
//! The kernel-backed production impl (`KernelSpawnHook`) stays in
//! `aura-agent-kernel` and implements this trait.

use async_trait::async_trait;
use aura_core::{AgentId, AgentPermissions, AgentToolPermissions, Hash};
use serde::{Deserialize, Serialize};

/// Specification for a child agent a `spawn_agent` call wants to create.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildAgentSpec {
    /// Display name for the new agent.
    pub name: String,
    /// Role tag (free-form; host applications use this).
    pub role: String,
    /// Permissions to attach to the new agent's `Identity`. Must already
    /// have been checked to be a strict subset of the caller's permissions
    /// before this hook is invoked.
    pub permissions: AgentPermissions,
    /// Optional per-tool override to stamp on the child's `Identity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_permissions: Option<AgentToolPermissions>,
    /// Parent/session per-tool override used for a final monotonic check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_permissions: Option<AgentToolPermissions>,
    /// Optional system-prompt override for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,
    /// Optional pre-assigned agent id. When `None` the hook generates one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preassigned_agent_id: Option<AgentId>,
}

/// Successful outcome of a `SpawnHook::spawn_child` call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnOutcome {
    /// The (fresh or pre-assigned) id of the new child agent.
    pub child_agent_id: AgentId,
    /// Optional host-application id for the created child. When present,
    /// callers should surface this id to users because it is the id accepted
    /// by product APIs such as aura-os `send_to_agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_agent_id: Option<String>,
    /// Hash of the `Delegate` transaction appended to the caller's log.
    pub delegate_tx_hash: Hash,
}

/// Errors a `SpawnHook` may surface.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// Underlying store / persistence error.
    #[error("store error: {0}")]
    Store(String),
    /// Serialization / payload error.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Any other failure.
    #[error("{0}")]
    Other(String),
}

/// Hook invoked by the `spawn_agent` tool to actually persist a new child
/// agent. Kept as a trait so tests can inject an in-memory recorder and
/// production can plug in the kernel-backed impl.
#[async_trait]
pub trait SpawnHook: Send + Sync {
    /// Create the child agent record + append the caller's `Delegate`
    /// transaction. `parent_agent_id` is the caller and `originating_user_id`
    /// is the end-user at the root of the chain.
    async fn spawn_child(
        &self,
        parent_agent_id: &AgentId,
        originating_user_id: Option<&str>,
        child: ChildAgentSpec,
    ) -> Result<SpawnOutcome, SpawnError>;
}

/// No-op hook used by unit tests. Generates an AgentId (or returns the
/// pre-assigned one) and reports a zero tx hash.
pub struct NoopSpawnHook;

#[async_trait]
impl SpawnHook for NoopSpawnHook {
    async fn spawn_child(
        &self,
        _parent_agent_id: &AgentId,
        _originating_user_id: Option<&str>,
        child: ChildAgentSpec,
    ) -> Result<SpawnOutcome, SpawnError> {
        let child_agent_id = child.preassigned_agent_id.unwrap_or_else(AgentId::generate);
        Ok(SpawnOutcome {
            child_agent_id,
            external_agent_id: None,
            delegate_tx_hash: Hash::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::AgentPermissions;

    #[tokio::test]
    async fn noop_hook_generates_child_id_when_absent() {
        let hook = NoopSpawnHook;
        let parent = AgentId::generate();
        let outcome = hook
            .spawn_child(
                &parent,
                Some("user-root"),
                ChildAgentSpec {
                    name: "c".into(),
                    role: "r".into(),
                    permissions: AgentPermissions::empty(),
                    tool_permissions: None,
                    parent_tool_permissions: None,
                    system_prompt_override: None,
                    preassigned_agent_id: None,
                },
            )
            .await
            .unwrap();
        assert_ne!(outcome.child_agent_id, parent);
    }

    #[tokio::test]
    async fn noop_hook_preserves_preassigned_id() {
        let hook = NoopSpawnHook;
        let parent = AgentId::generate();
        let pre = AgentId::generate();
        let outcome = hook
            .spawn_child(
                &parent,
                None,
                ChildAgentSpec {
                    name: "c".into(),
                    role: "r".into(),
                    permissions: AgentPermissions::empty(),
                    tool_permissions: None,
                    parent_tool_permissions: None,
                    system_prompt_override: None,
                    preassigned_agent_id: Some(pre),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.child_agent_id, pre);
    }
}
