//! Phase 7b: each override field on the task tool input must be
//! threaded through to the derived [`aura_agent_subagent::SubagentSpec`] +
//! [`aura_agent_subagent::OverrideManifest`]. The manifest is written
//! into the `RecordKind::SubagentSpawn` audit payload; this test
//! inspects the audit log to assert the round-trip.
//!
//! Phase 10 carve-out 3: the on-disk wire format moved from
//! `TransactionType::System` + JSON discriminator to the typed
//! `TransactionType::SubagentSpawn` variant; the payload no
//! longer carries the `kind: "subagent_spawn"` field. The
//! [`applied_fields`] scanner below is updated to match.
//!
//! Phase B / Commit 3 / Step 3a moved this test from
//! `aura-runtime/tests/` to `aura-fleet-subagent/tests/` because the
//! concrete dispatcher (`FleetSubagentDispatcher`, renamed from
//! `RuntimeSubagentDispatch`) now lives here.

mod common;

use aura_agent_subagent::OverriddenField;
use aura_core_types::{
    AgentId, AgentPermissions, AgentScope, Capability, SubagentBudget, SubagentDispatchRequest,
    SubagentExit, UserToolDefaults,
};
use aura_tools::SubagentDispatchHook;
use serde_json::Value;
use std::sync::Arc;

use common::build_dispatch_with_response;

fn base_request(parent_agent_id: AgentId) -> SubagentDispatchRequest {
    SubagentDispatchRequest {
        parent_agent_id,
        subagent_type: "explore".into(),
        prompt: "override".into(),
        originating_user_id: Some("override-user".into()),
        parent_chain: Vec::new(),
        model_override: None,
        system_prompt_addendum: None,
        parent_permissions: AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent],
        },
        parent_tool_permissions: None,
        user_tool_defaults: UserToolDefaults::full_access(),
        tool_call_id: None,
        parent_mode: None,
        parent_kernel_mode: None,
        parent_model_id: None,
        override_mode: None,
        override_permissions: None,
        override_tool_subset: None,
        override_isolation_id: None,
        override_budget: None,
        spawn_mode: None,
        council_index: None,
    }
}

fn applied_fields(
    store: &Arc<dyn aura_store_db::Store>,
    parent_agent_id: AgentId,
) -> Vec<OverriddenField> {
    let records = store.scan_record(parent_agent_id, 1, 100).expect("scan");
    for entry in records {
        if entry.tx.tx_type != aura_core_types::TransactionType::SubagentSpawn {
            continue;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&entry.tx.payload) else {
            continue;
        };
        let manifest = payload
            .get("override_manifest")
            .cloned()
            .unwrap_or(Value::Null);
        if let Ok(parsed) =
            serde_json::from_value::<aura_agent_subagent::OverrideManifest>(manifest)
        {
            return parsed.applied;
        }
    }
    Vec::new()
}

#[tokio::test]
async fn override_budget_threaded_into_manifest() {
    let (dispatch, store, _d, _w) = build_dispatch_with_response("override e2e child output");
    let parent_agent_id = AgentId::generate();
    let mut req = base_request(parent_agent_id);
    req.override_budget = Some(SubagentBudget {
        max_iterations: 10,
        max_tokens: Some(2_000),
        timeout_ms: 60_000,
    });
    let result = SubagentDispatchHook::dispatch(&dispatch, req)
        .await
        .expect("dispatch");
    assert!(matches!(result.exit, SubagentExit::Completed));
    let applied = applied_fields(&store, parent_agent_id);
    assert!(
        applied.iter().any(|f| matches!(f, OverriddenField::Budget)),
        "manifest must contain Budget; got {applied:?}"
    );
}

#[tokio::test]
async fn override_tool_subset_threaded_into_manifest() {
    let (dispatch, store, _d, _w) = build_dispatch_with_response("override e2e child output");
    let parent_agent_id = AgentId::generate();
    let mut req = base_request(parent_agent_id);
    req.override_tool_subset = Some(vec!["read_file".into()]);
    let result = SubagentDispatchHook::dispatch(&dispatch, req)
        .await
        .expect("dispatch");
    assert!(matches!(result.exit, SubagentExit::Completed));
    let applied = applied_fields(&store, parent_agent_id);
    assert!(
        applied
            .iter()
            .any(|f| matches!(f, OverriddenField::ToolSubset { count } if *count == 1)),
        "manifest must contain ToolSubset; got {applied:?}"
    );
}

#[tokio::test]
async fn override_isolation_id_threaded_into_manifest() {
    let (dispatch, store, _d, _w) = build_dispatch_with_response("override e2e child output");
    let parent_agent_id = AgentId::generate();
    let mut req = base_request(parent_agent_id);
    req.override_isolation_id = Some("worktree-42".into());
    let result = SubagentDispatchHook::dispatch(&dispatch, req)
        .await
        .expect("dispatch");
    assert!(matches!(result.exit, SubagentExit::Completed));
    let applied = applied_fields(&store, parent_agent_id);
    assert!(
        applied
            .iter()
            .any(|f| matches!(f, OverriddenField::IsolationId(id) if id == "worktree-42")),
        "manifest must contain IsolationId; got {applied:?}"
    );
}

#[tokio::test]
async fn model_override_threaded_into_manifest() {
    let (dispatch, store, _d, _w) = build_dispatch_with_response("override e2e child output");
    let parent_agent_id = AgentId::generate();
    let mut req = base_request(parent_agent_id);
    req.model_override = Some("custom-model-x".into());
    req.parent_model_id = Some("parent-model".into());
    let result = SubagentDispatchHook::dispatch(&dispatch, req)
        .await
        .expect("dispatch");
    assert!(matches!(result.exit, SubagentExit::Completed));
    let applied = applied_fields(&store, parent_agent_id);
    assert!(
        applied.iter().any(|f| matches!(
            f,
            OverriddenField::ModelId { to, .. } if to == "custom-model-x"
        )),
        "manifest must contain ModelId; got {applied:?}"
    );
}

#[tokio::test]
async fn system_prompt_addendum_threaded_into_manifest() {
    let (dispatch, store, _d, _w) = build_dispatch_with_response("override e2e child output");
    let parent_agent_id = AgentId::generate();
    let mut req = base_request(parent_agent_id);
    req.system_prompt_addendum = Some("be terse".into());
    let result = SubagentDispatchHook::dispatch(&dispatch, req)
        .await
        .expect("dispatch");
    assert!(matches!(result.exit, SubagentExit::Completed));
    let applied = applied_fields(&store, parent_agent_id);
    assert!(
        applied
            .iter()
            .any(|f| matches!(f, OverriddenField::SystemPromptAddendum { chars } if *chars > 0)),
        "manifest must contain SystemPromptAddendum; got {applied:?}"
    );
}
