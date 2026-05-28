//! Phase 7b idempotent dedupe: spawning twice with the same
//! `(parent_agent_id, tool_call_id)` within the dedupe window must
//! produce ONE child + ONE audit record. The second call returns the
//! cached outcome verbatim.

mod common;

use std::sync::Arc;

use aura_agent_subagent::SubagentOverrides;
use aura_core::TransactionType;
use aura_core_modes::{AgentMode, SpawnMode};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnHandle, SpawnRequest,
    SubagentSpawnRecordPayload,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test]
async fn same_parent_and_tool_call_id_produces_one_child_and_one_audit_record() {
    let parent = parent_at(AgentMode::Agent, 0);
    let parent_id = parent.agent_id;
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::new());

    let spawner = FleetSpawner::with_default_derivation(
        store.clone(),
        registry,
        quota,
        leases,
        orphans,
        runner.clone(),
        FleetSpawnerConfig::default(),
    );

    let make_request = || SpawnRequest {
        parent: parent.clone(),
        overrides: SubagentOverrides::default(),
        prompt: "dedupe-me".to_string(),
        originating_user_id: Some("user".to_string()),
        tool_call_id: Some("call-1".to_string()),
        cancellation: None,
    };

    let first = spawner
        .spawn(make_request(), SpawnMode::Wait)
        .await
        .expect("first spawn ok");
    let second = spawner
        .spawn(make_request(), SpawnMode::Wait)
        .await
        .expect("second spawn ok (dedupe)");

    let SpawnHandle::Completed(first_result) = first else {
        panic!("expected Completed for first");
    };
    let SpawnHandle::Completed(second_result) = second else {
        panic!("expected Completed for second (dedupe)");
    };

    assert_eq!(
        first_result.child_agent_id, second_result.child_agent_id,
        "dedupe must return the SAME child agent id"
    );
    assert_eq!(
        first_result.final_message, second_result.final_message,
        "dedupe must return the SAME result"
    );

    assert_eq!(
        runner.invocation_count(),
        1,
        "dedupe must NOT invoke the runner a second time"
    );

    let entries = store
        .scan_record(parent_id, 1, 20)
        .expect("scan parent record log");
    let spawn_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.tx.tx_type == TransactionType::SubagentSpawn)
        .filter(|entry| {
            serde_json::from_slice::<SubagentSpawnRecordPayload>(&entry.tx.payload).is_ok()
        })
        .collect();
    assert_eq!(
        spawn_entries.len(),
        1,
        "dedupe must NOT write a second SubagentSpawn audit record"
    );
}
