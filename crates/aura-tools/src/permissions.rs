//! Pure tool-permission helpers consumed by the gateway-side HTTP
//! handlers.
//!
//! Phase B / Commit 3 / Step 6 relocated these helpers from
//! `aura-runtime/src/tool_permissions.rs` to the exec layer so the
//! gateway no longer owns the underlying validation logic. The
//! router-level HTTP handler shape lives in `aura-runtime` still; it
//! calls into these helpers.
//!
//! Three pure helpers move here:
//! - [`validate_user_defaults`] — verify every key in
//!   `UserDefaultMode::DefaultPermissions::per_tool` resolves to a tool
//!   the catalog actually knows about.
//! - [`validate_agent_tool_permissions`] — same shape, for
//!   per-agent overrides.
//! - [`load_agent_tool_context`] — replay the agent's record-log tail
//!   to recover the most recent [`AgentToolContext`] (current
//!   tool-permissions override, agent permissions, originating user
//!   id). Used by both the chat WS path (`session/helpers.rs`) and
//!   the REST `/agents/:id/tool_permissions` PUT handler.

use crate::{catalog::ToolProfile, ToolCatalog};
use aura_core::{
    AgentId, AgentPermissions, AgentToolPermissions, Identity, RecordEntry, UserDefaultMode,
    UserToolDefaults,
};
use aura_store::Store;
use std::collections::HashSet;
use std::sync::Arc;

/// Replayed snapshot of the per-agent tool-permission state used by
/// the gateway-side router handlers.
#[derive(Debug, Clone)]
pub struct AgentToolContext {
    /// Most recent [`AgentToolPermissions`] override recorded on the
    /// agent's log (either via an `Identity` System record or an
    /// explicit `agent_tool_permissions` System record). `None` when
    /// no override has ever been applied.
    pub tool_permissions: Option<AgentToolPermissions>,
    /// Most recent [`AgentPermissions`] bundle recorded on the
    /// agent's log. Defaults to [`AgentPermissions::empty`] when the
    /// agent has no Identity record yet.
    pub agent_permissions: AgentPermissions,
    /// Originating user id captured at session start. Used to look
    /// up the appropriate [`UserToolDefaults`] for downstream
    /// effective-state computation.
    pub originating_user_id: Option<String>,
}

/// Validate a [`UserToolDefaults`] against the canonical catalog —
/// every per-tool override key must resolve to a tool the catalog
/// knows about.
pub fn validate_user_defaults(
    defaults: &UserToolDefaults,
    catalog: &ToolCatalog,
) -> Result<(), String> {
    if let UserDefaultMode::DefaultPermissions { per_tool, .. } = &defaults.mode {
        validate_tool_names(per_tool.keys().map(String::as_str), catalog)?;
    }
    Ok(())
}

/// Validate an [`AgentToolPermissions`] override against the canonical
/// catalog — every per-tool override key must resolve to a tool the
/// catalog knows about.
pub fn validate_agent_tool_permissions(
    permissions: &AgentToolPermissions,
    catalog: &ToolCatalog,
) -> Result<(), String> {
    validate_tool_names(permissions.per_tool.keys().map(String::as_str), catalog)
}

fn validate_tool_names<'a>(
    names: impl Iterator<Item = &'a str>,
    catalog: &ToolCatalog,
) -> Result<(), String> {
    let known = catalog
        .tools_for_profile_with_permissions(ToolProfile::Agent, None)
        .into_iter()
        .map(|tool| tool.name)
        .collect::<HashSet<_>>();
    for name in names {
        if !known.contains(name) {
            return Err(format!("unknown tool '{name}'"));
        }
    }
    Ok(())
}

/// Replay the most recent ~256 record entries for `agent_id` and
/// rebuild the [`AgentToolContext`] snapshot. Used by the gateway's
/// chat-WS bootstrap path and by the REST tool-permissions handlers.
pub fn load_agent_tool_context(
    store: &Arc<dyn Store>,
    agent_id: AgentId,
) -> Result<AgentToolContext, String> {
    let head = store
        .get_head_seq(agent_id)
        .map_err(|e| format!("get_head_seq: {e}"))?;
    if head == 0 {
        return Ok(AgentToolContext {
            tool_permissions: None,
            agent_permissions: AgentPermissions::empty(),
            originating_user_id: None,
        });
    }
    let from_seq = head.saturating_sub(255).max(1);
    let entries = store
        .scan_record(agent_id, from_seq, 256)
        .map_err(|e| format!("scan_record: {e}"))?;
    Ok(context_from_entries(entries))
}

fn context_from_entries(entries: Vec<RecordEntry>) -> AgentToolContext {
    let mut originating_user_id = None;
    let mut tool_permissions = None;
    let mut agent_permissions = AgentPermissions::empty();
    for entry in entries {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&entry.tx.payload) else {
            continue;
        };
        if let Some(parsed) = value
            .get("identity")
            .and_then(|v| serde_json::from_value::<Identity>(v.clone()).ok())
        {
            tool_permissions = parsed.tool_permissions.clone();
            agent_permissions = parsed.permissions;
        }
        if let Some(user_id) = value.get("originating_user_id").and_then(|v| v.as_str()) {
            originating_user_id = Some(user_id.to_string());
        }
        if value.get("kind").and_then(|v| v.as_str()) == Some("agent_tool_permissions") {
            tool_permissions = value
                .get("tool_permissions")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
        }
    }
    AgentToolContext {
        tool_permissions,
        agent_permissions,
        originating_user_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{AgentScope, Capability, ToolState, Transaction, TransactionType};
    use bytes::Bytes;
    use std::collections::BTreeMap;

    fn defaults(entries: &[(&str, ToolState)], fallback: ToolState) -> UserToolDefaults {
        UserToolDefaults::default_permissions(
            entries
                .iter()
                .map(|(tool, state)| ((*tool).to_string(), *state))
                .collect(),
            fallback,
        )
    }

    fn overrides(entries: &[(&str, ToolState)]) -> AgentToolPermissions {
        AgentToolPermissions {
            per_tool: entries
                .iter()
                .map(|(tool, state)| ((*tool).to_string(), *state))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn validate_user_defaults_rejects_unknown_tool_names() {
        let catalog = ToolCatalog::new();
        let unknown = defaults(&[("not_a_real_tool", ToolState::Allow)], ToolState::Deny);

        let err = validate_user_defaults(&unknown, &catalog).expect_err("unknown tool rejected");
        assert!(err.contains("unknown tool 'not_a_real_tool'"));
    }

    #[test]
    fn validate_agent_permissions_accepts_catalog_tool_names() {
        let catalog = ToolCatalog::new();
        let permissions = overrides(&[("read_file", ToolState::Ask)]);

        validate_agent_tool_permissions(&permissions, &catalog).expect("known tool accepted");
    }

    #[test]
    fn context_from_entries_recovers_identity_agent_permissions() {
        let agent_id = AgentId::new([7; 32]);
        let permissions = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let identity = Identity::new("0://Agent07", "Agent07")
            .with_permissions(permissions.clone())
            .with_tool_permissions(Some(overrides(&[("read_file", ToolState::Ask)])));
        let payload = serde_json::to_vec(&serde_json::json!({
            "identity": identity,
            "originating_user_id": "user-1",
        }))
        .expect("serialize identity payload");
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::System,
            Bytes::from(payload),
            None,
        );
        let entry = RecordEntry::builder(1, tx).build();

        let context = context_from_entries(vec![entry]);

        assert_eq!(context.agent_permissions, permissions);
        assert_eq!(context.originating_user_id.as_deref(), Some("user-1"));
        assert_eq!(
            context
                .tool_permissions
                .as_ref()
                .and_then(|perms| perms.per_tool.get("read_file")),
            Some(&ToolState::Ask)
        );
    }
}
