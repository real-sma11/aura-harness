//! Kernel-backed `SpawnHook` impl (phase 5 part 2).
//!
//! The [`SpawnHook`] trait, [`ChildAgentSpec`], [`SpawnOutcome`],
//! [`SpawnError`], and the default [`NoopSpawnHook`] now live in the
//! exec-layer crate `aura-exec-traits` so the `aura-tools` exec crate can
//! consume them without an upward dependency on this agent-layer kernel.
//!
//! [`KernelSpawnHook`] — the production wiring — stays here because it needs
//! the kernel's `Store` + context-hashing internals. It writes the new
//! Identity as a `System` transaction on the child's record log and writes a
//! `Delegate` transaction on the caller's record log carrying
//! `parent_agent_id` + `originating_user_id`.

use async_trait::async_trait;
use aura_core::{
    resolve_effective_permission, AgentId, AgentPermissions, AgentToolPermissions, Identity,
    ToolState, Transaction, TransactionType, UserToolDefaults,
};
use aura_exec_traits::{ChildAgentSpec, SpawnError, SpawnHook, SpawnOutcome};
use aura_store::Store;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Kernel-backed hook that writes the child `Identity` (as a `System`
/// transaction on the child's record log) and appends a `Delegate`
/// transaction on the caller's record log.
pub struct KernelSpawnHook {
    store: Arc<dyn Store>,
}

impl KernelSpawnHook {
    /// Construct a new kernel-backed spawn hook.
    #[must_use]
    pub const fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildIdentityPayload {
    identity: Identity,
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system_prompt_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    originating_user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DelegateSpawnPayload {
    kind: &'static str,
    parent_agent_id: AgentId,
    child_agent_id: AgentId,
    name: String,
    role: String,
    permissions: AgentPermissions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_permissions: Option<AgentToolPermissions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    originating_user_id: Option<String>,
}

#[async_trait]
impl SpawnHook for KernelSpawnHook {
    async fn spawn_child(
        &self,
        parent_agent_id: &AgentId,
        originating_user_id: Option<&str>,
        child: ChildAgentSpec,
    ) -> Result<SpawnOutcome, SpawnError> {
        let child_agent_id = child.preassigned_agent_id.unwrap_or_else(AgentId::generate);

        let user_default = match originating_user_id {
            Some(user_id) => self
                .store
                .get_user_tool_defaults(user_id)
                .map_err(|e| SpawnError::Store(format!("get_user_tool_defaults: {e}")))?
                .unwrap_or_default(),
            None => UserToolDefaults::default(),
        };
        let parent_tool_permissions = child.parent_tool_permissions.clone().or_else(|| {
            self.latest_identity(parent_agent_id)
                .and_then(|id| id.tool_permissions)
        });
        enforce_tool_subset(
            &user_default,
            parent_tool_permissions.as_ref(),
            child.tool_permissions.as_ref(),
        )?;

        let zns_id = format!("0://spawn/{}", child_agent_id.to_hex());
        let mut identity = Identity::new(&zns_id, &child.name);
        identity.agent_id = child_agent_id;
        identity = identity.with_permissions(child.permissions.clone());
        identity = identity.with_tool_permissions(child.tool_permissions.clone());

        let identity_payload = ChildIdentityPayload {
            identity,
            role: child.role.clone(),
            system_prompt_override: child.system_prompt_override.clone(),
            parent_agent_id: Some(*parent_agent_id),
            originating_user_id: originating_user_id.map(ToString::to_string),
        };
        let identity_bytes = serde_json::to_vec(&identity_payload)
            .map_err(|e| SpawnError::Serialization(format!("child identity: {e}")))?;

        let child_tx = Transaction::new_chained(
            child_agent_id,
            TransactionType::System,
            Bytes::from(identity_bytes),
            None,
        );
        let child_seq = self
            .store
            .get_head_seq(child_agent_id)
            .map_err(|e| SpawnError::Store(format!("get_head_seq(child): {e}")))?
            + 1;
        let child_ctx_hash = crate::context::hash_tx_with_window(&child_tx, &[])
            .map_err(|e| SpawnError::Other(format!("context hash(child): {e}")))?;
        let child_entry = aura_core::RecordEntry::builder(child_seq, child_tx)
            .context_hash(child_ctx_hash)
            .build();
        self.store
            .append_entry_direct(child_agent_id, child_seq, &child_entry)
            .map_err(|e| SpawnError::Store(format!("append_entry_direct(child): {e}")))?;

        let delegate_payload = DelegateSpawnPayload {
            kind: "spawn_agent",
            parent_agent_id: *parent_agent_id,
            child_agent_id,
            name: child.name.clone(),
            role: child.role.clone(),
            permissions: child.permissions.clone(),
            tool_permissions: child.tool_permissions.clone(),
            originating_user_id: originating_user_id.map(ToString::to_string),
        };
        let delegate_bytes = serde_json::to_vec(&delegate_payload)
            .map_err(|e| SpawnError::Serialization(format!("delegate payload: {e}")))?;

        let delegate_tx = Transaction::new_chained(
            *parent_agent_id,
            TransactionType::System,
            Bytes::from(delegate_bytes),
            None,
        );
        let delegate_hash = delegate_tx.hash;
        let parent_seq = self
            .store
            .get_head_seq(*parent_agent_id)
            .map_err(|e| SpawnError::Store(format!("get_head_seq(parent): {e}")))?
            + 1;
        let parent_ctx_hash = crate::context::hash_tx_with_window(&delegate_tx, &[])
            .map_err(|e| SpawnError::Other(format!("context hash(parent): {e}")))?;
        let parent_entry = aura_core::RecordEntry::builder(parent_seq, delegate_tx)
            .context_hash(parent_ctx_hash)
            .build();
        self.store
            .append_entry_direct(*parent_agent_id, parent_seq, &parent_entry)
            .map_err(|e| SpawnError::Store(format!("append_entry_direct(parent): {e}")))?;

        Ok(SpawnOutcome {
            child_agent_id,
            external_agent_id: None,
            delegate_tx_hash: delegate_hash,
        })
    }
}

impl KernelSpawnHook {
    fn latest_identity(&self, agent_id: &AgentId) -> Option<Identity> {
        let head = self.store.get_head_seq(*agent_id).ok()?;
        let from_seq = head.saturating_sub(256).saturating_add(1);
        let entries = self.store.scan_record(*agent_id, from_seq, 256).ok()?;
        entries
            .iter()
            .rev()
            .find_map(|entry| identity_from_payload(&entry.tx.payload))
    }
}

fn identity_from_payload(payload: &[u8]) -> Option<Identity> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    value
        .get("identity")
        .and_then(|identity| serde_json::from_value(identity.clone()).ok())
}

fn enforce_tool_subset(
    user_default: &UserToolDefaults,
    parent: Option<&AgentToolPermissions>,
    child: Option<&AgentToolPermissions>,
) -> Result<(), SpawnError> {
    let Some(child) = child else {
        return Ok(());
    };
    for (tool, child_state) in &child.per_tool {
        let parent_state = resolve_effective_permission(user_default, parent, tool);
        if !child_state.is_subset_of(parent_state) {
            return Err(SpawnError::Other(format!(
                "tool permissions: requested '{tool}'={} exceeds parent effective {}",
                state_label(*child_state),
                state_label(parent_state)
            )));
        }
    }
    Ok(())
}

fn state_label(state: ToolState) -> &'static str {
    match state {
        ToolState::Allow => "on",
        ToolState::Deny => "off",
        ToolState::Ask => "ask",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::AgentPermissions;

    #[tokio::test]
    async fn kernel_hook_persists_child_and_delegate_entries() {
        use aura_store::{RocksStore, Store};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.path(), false).unwrap());
        let hook = KernelSpawnHook::new(store.clone());

        let parent = AgentId::generate();
        let outcome = hook
            .spawn_child(
                &parent,
                Some("user-root"),
                ChildAgentSpec {
                    name: "worker".into(),
                    role: "builder".into(),
                    permissions: AgentPermissions::ceo_preset(),
                    tool_permissions: None,
                    parent_tool_permissions: None,
                    system_prompt_override: Some("be fast".into()),
                    preassigned_agent_id: None,
                },
            )
            .await
            .unwrap();

        // Child record log got a single System entry carrying the identity.
        assert_eq!(store.get_head_seq(outcome.child_agent_id).unwrap(), 1);
        let child_entries = store.scan_record(outcome.child_agent_id, 1, 10).unwrap();
        assert_eq!(child_entries.len(), 1);
        assert_eq!(
            child_entries[0].tx.tx_type,
            aura_core::TransactionType::System
        );

        // Parent log got the Delegate marker.
        assert_eq!(store.get_head_seq(parent).unwrap(), 1);
        let parent_entries = store.scan_record(parent, 1, 10).unwrap();
        assert_eq!(parent_entries.len(), 1);
        let payload: serde_json::Value =
            serde_json::from_slice(&parent_entries[0].tx.payload).unwrap();
        assert_eq!(payload["kind"], "spawn_agent");
        assert_eq!(payload["originating_user_id"], "user-root");
        assert_eq!(
            payload["child_agent_id"],
            serde_json::json!(outcome.child_agent_id)
        );
        assert_ne!(outcome.delegate_tx_hash, aura_core::Hash::default());
    }

    #[tokio::test]
    async fn kernel_hook_writes_nonzero_context_hashes() {
        use aura_store::{RocksStore, Store};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.path(), false).unwrap());
        let hook = KernelSpawnHook::new(store.clone());

        let parent = AgentId::generate();
        let outcome = hook
            .spawn_child(
                &parent,
                Some("user-root"),
                ChildAgentSpec {
                    name: "c".into(),
                    role: "r".into(),
                    permissions: AgentPermissions::ceo_preset(),
                    tool_permissions: None,
                    parent_tool_permissions: None,
                    system_prompt_override: None,
                    preassigned_agent_id: None,
                },
            )
            .await
            .unwrap();

        // Regression guard for Invariant §6: spawn hook must compute a real
        // context_hash, never leave it zeroed.
        let child_entries = store.scan_record(outcome.child_agent_id, 1, 10).unwrap();
        assert_ne!(
            child_entries[0].context_hash,
            aura_core::ContextHash::zero(),
            "child entry must have a non-zero context_hash"
        );
        let parent_entries = store.scan_record(parent, 1, 10).unwrap();
        assert_ne!(
            parent_entries[0].context_hash,
            aura_core::ContextHash::zero(),
            "parent delegate entry must have a non-zero context_hash"
        );
    }
}
