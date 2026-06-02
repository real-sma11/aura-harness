//! Phase 7a invariant: two children spawned concurrently from the
//! SAME parent get correctly-ordered, gap-free `seq` numbers in the
//! parent's audit log under the per-parent lease.
//!
//! Today's `spawn_lock` enforces the same property by serialising
//! every parent in the daemon. The lease swap must preserve this
//! per-parent guarantee while removing the cross-parent contention
//! (covered by the sibling test `parent_lease_independent_parents`).

mod common;

use std::sync::Arc;

use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, SpawnMode};
use aura_core_types::TransactionType;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnRequest, SubagentSpawnRecordPayload,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test]
async fn two_concurrent_spawns_from_same_parent_produce_monotone_seqs() {
    let parent = parent_at(AgentMode::Agent, 0);
    let parent_id = parent.agent_id;
    let (store, _keep) = open_test_store();
    let (orphans, _orphan_dir) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::new());
    let spawner = Arc::new(FleetSpawner::with_default_derivation(
        store.clone(),
        registry.clone(),
        quota.clone(),
        leases.clone(),
        orphans,
        runner.clone(),
        FleetSpawnerConfig::default(),
    ));

    let a = spawner.clone();
    let p_a = parent.clone();
    let handle_a = tokio::spawn(async move {
        a.spawn(
            SpawnRequest {
                parent: p_a,
                overrides: SubagentOverrides::default(),
                prompt: "first".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            SpawnMode::Wait,
        )
        .await
    });
    let b = spawner;
    let p_b = parent.clone();
    let handle_b = tokio::spawn(async move {
        b.spawn(
            SpawnRequest {
                parent: p_b,
                overrides: SubagentOverrides::default(),
                prompt: "second".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            SpawnMode::Wait,
        )
        .await
    });

    handle_a.await.unwrap().expect("first spawn ok");
    handle_b.await.unwrap().expect("second spawn ok");

    assert_eq!(runner.invocation_count(), 2);
    // Phase 7b: RAII BudgetTickets are dropped after the Wait child
    // completes, so post-spawn outstanding counters return to zero.
    assert_eq!(quota.outstanding(), 0, "tickets must release on drop");

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
        2,
        "expected exactly two SubagentSpawn audit records"
    );
    assert_eq!(spawn_entries[0].seq, 1, "first record seq must be 1");
    assert_eq!(spawn_entries[1].seq, 2, "second record seq must be 2");
    assert_eq!(
        spawn_entries[1].seq - spawn_entries[0].seq,
        1,
        "seqs must be strictly monotone with no gaps"
    );
}
