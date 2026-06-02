//! Gateway-side tool-permission helpers.
//!
//! Phase B / Commit 3 / Step 6 moved the pure helpers
//! (`load_agent_tool_context`, `validate_user_defaults`,
//! `validate_agent_tool_permissions`) into
//! [`aura_tools::permissions`]. What stays here:
//!
//! - [`EffectiveToolInfo`] — the wire-format DTO returned by
//!   `GET /v1/agents/:id/tools`. Carries `protocol::ToolStateWire` so
//!   the protocol crate stays the only place that knows the wire
//!   serialization shape.
//! - [`effective_tool_definitions`] / [`effective_tool_infos`] — fold
//!   the user / agent / kind permission stack into per-tool
//!   [`aura_model_reasoner::ToolDefinition`] (or [`EffectiveToolInfo`])
//!   lists. Used by the chat-WS bootstrap path + the REST tools
//!   handler.
//! - [`enforce_monotonic_update`] — reject attempts to widen an
//!   existing per-tool override past the user default.
//! - [`append_agent_tool_permissions_entry`] — append the System
//!   record for an HTTP-driven `agent_tool_permissions` update.
//!   Acquires the engine-side [`crate::scheduler::Scheduler`]
//!   processing claim so the write serializes with the scheduler's
//!   inbox-drain on the same agent.

use crate::protocol;
use crate::scheduler::Scheduler;
use aura_core_types::{
    installed_integrations_satisfy, AgentId, AgentPermissions, AgentToolPermissions,
    InstalledIntegrationDefinition, InstalledToolDefinition, ToolState, Transaction,
    TransactionType, UserToolDefaults,
};
use aura_model_reasoner::ToolDefinition;
use aura_store_db::Store;
use aura_tools::{catalog::ToolProfile, ToolCatalog, ToolConfig};
use bytes::Bytes;
use std::collections::HashSet;
use std::sync::Arc;

/// Re-export the moved pure helpers under the legacy in-crate import
/// path so the gateway handlers continue to read straight against
/// `crate::tool_permissions::*`.
pub(crate) use aura_tools::permissions::{
    load_agent_tool_context, validate_agent_tool_permissions, validate_user_defaults,
};

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct EffectiveToolInfo {
    pub name: String,
    pub description: String,
    pub effective_state: protocol::ToolStateWire,
}

pub(crate) fn effective_tool_definitions(
    catalog: &ToolCatalog,
    tool_config: &ToolConfig,
    installed_tools: &[InstalledToolDefinition],
    installed_integrations: &[InstalledIntegrationDefinition],
    user_default: &UserToolDefaults,
    agent_override: Option<&AgentToolPermissions>,
    agent_permissions: Option<&AgentPermissions>,
) -> Vec<(ToolDefinition, ToolState)> {
    let mut seen = HashSet::new();
    let mut tools = Vec::new();
    for tool in
        catalog.visible_tools_with_permissions(ToolProfile::Agent, tool_config, agent_permissions)
    {
        let state =
            aura_core_types::resolve_effective_permission(user_default, agent_override, &tool.name);
        if state != ToolState::Deny && seen.insert(tool.name.clone()) {
            tools.push((tool, state));
        }
    }
    for tool in installed_tools {
        if let Some(requirement) = tool.required_integration.as_ref() {
            if !installed_integrations_satisfy(requirement, installed_integrations) {
                continue;
            }
        }
        let state =
            aura_core_types::resolve_effective_permission(user_default, agent_override, &tool.name);
        if state != ToolState::Deny && seen.insert(tool.name.clone()) {
            tools.push((
                ToolDefinition::new(&tool.name, &tool.description, tool.input_schema.clone()),
                state,
            ));
        }
    }
    tools
}

pub(crate) fn effective_tool_infos(
    catalog: &ToolCatalog,
    tool_config: &ToolConfig,
    user_default: &UserToolDefaults,
    agent_override: Option<&AgentToolPermissions>,
    agent_permissions: Option<&AgentPermissions>,
) -> Vec<EffectiveToolInfo> {
    catalog
        .visible_tools_with_permissions(ToolProfile::Agent, tool_config, agent_permissions)
        .into_iter()
        .filter_map(|tool| {
            let state = aura_core_types::resolve_effective_permission(
                user_default,
                agent_override,
                &tool.name,
            );
            (state != ToolState::Deny).then(|| EffectiveToolInfo {
                name: tool.name,
                description: tool.description,
                effective_state: protocol::tool_state_to_wire(state),
            })
        })
        .collect()
}

/// Append an `agent_tool_permissions` System entry to the agent's log.
///
/// Acquires the scheduler's store-backed processing claim before the
/// `append_entry_direct` call so this HTTP-driven write serializes with
/// the scheduler's inbox-drain on the same agent. Without this the
/// single-writer guarantee can be violated if a scheduler tick is
/// running concurrently.
pub(crate) async fn append_agent_tool_permissions_entry(
    store: &Arc<dyn Store>,
    scheduler: &Arc<Scheduler>,
    agent_id: AgentId,
    permissions: &AgentToolPermissions,
) -> Result<Transaction, String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "kind": "agent_tool_permissions",
        "agent_id": agent_id,
        "tool_permissions": permissions,
    }))
    .map_err(|e| format!("serialize agent_tool_permissions: {e}"))?;
    let tx = Transaction::new_chained(
        agent_id,
        TransactionType::System,
        Bytes::from(payload),
        None,
    );

    // Hold the processing claim for the entire read-modify-write window so a
    // concurrent scheduler drain cannot wedge a different entry at the same
    // seq between our `get_head_seq` and `append_entry_direct`. The
    // scheduler claim is a runtime-side lock; the actual entry build +
    // store write is delegated to `aura_agent_kernel::write_system_record`
    // so this code path no longer bypasses the kernel crate (Phase 6a).
    let _claim = scheduler
        .processing_claim(agent_id)
        .await
        .map_err(|e| format!("claim agent processing: {e}"))?;

    aura_agent_kernel::write_system_record(store, agent_id, tx.clone())
        .map_err(|e| format!("write_system_record: {e}"))?;
    Ok(tx)
}

pub(crate) fn enforce_monotonic_update(
    user_default: &UserToolDefaults,
    current: Option<&AgentToolPermissions>,
    next: &AgentToolPermissions,
) -> Result<(), String> {
    for (tool, next_state) in &next.per_tool {
        let current_state =
            aura_core_types::resolve_effective_permission(user_default, current, tool);
        if !next_state.is_subset_of(current_state) {
            return Err(format!(
                "tool '{tool}' cannot be widened from {} to {}",
                state_label(current_state),
                state_label(*next_state)
            ));
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
    use aura_core_types::ToolState;
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
    fn monotonic_update_rejects_widening_and_allows_narrowing() {
        let user_default = defaults(&[("run_command", ToolState::Ask)], ToolState::Allow);
        let current = overrides(&[("read_file", ToolState::Ask)]);

        let widening = overrides(&[
            ("read_file", ToolState::Allow),
            ("run_command", ToolState::Allow),
        ]);
        let err = enforce_monotonic_update(&user_default, Some(&current), &widening)
            .expect_err("widening should be rejected");
        assert!(err.contains("cannot be widened"));

        let narrowing = overrides(&[
            ("read_file", ToolState::Deny),
            ("run_command", ToolState::Deny),
        ]);
        enforce_monotonic_update(&user_default, Some(&current), &narrowing)
            .expect("narrowing should be accepted");
    }
}
