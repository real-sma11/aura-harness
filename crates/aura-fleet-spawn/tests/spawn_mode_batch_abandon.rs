//! Phase 7b `JoinPolicy::Abandon`: 3 children spawned as a fire-and-forget
//! batch; parent gets the agent ids immediately and each child has an
//! orphan record persisted under the configured root.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, JoinPolicy};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    BatchOutcome, FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnRequest,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test]
async fn abandon_returns_immediately_and_orphans_every_child() {
    let parent = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::with_delay(Duration::from_millis(200)));

    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry,
        quota,
        leases,
        orphans.clone(),
        runner.clone(),
        FleetSpawnerConfig::default(),
    );

    let make = |prompt: &str| SpawnRequest {
        parent: parent.clone(),
        overrides: SubagentOverrides::default(),
        prompt: prompt.to_string(),
        originating_user_id: Some("user".to_string()),
        tool_call_id: None,
        cancellation: None,
    };

    let started = std::time::Instant::now();
    let batch = spawner
        .spawn_batch(vec![make("a"), make("b"), make("c")], JoinPolicy::Abandon)
        .await
        .expect("batch ok");

    let ids = batch.agent_ids.clone();
    assert_eq!(ids.len(), 3);

    let outcome = batch.join().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "abandon batch must return immediately (got {elapsed:?})"
    );

    match outcome {
        BatchOutcome::Abandoned(abandoned_ids) => {
            assert_eq!(abandoned_ids.len(), 3);
            for id in &ids {
                assert!(abandoned_ids.contains(id));
            }
        }
        other => panic!("expected Abandoned, got {other:?}"),
    }

    // Every child must have a durable orphan record now.
    for id in ids {
        let record = orphans
            .load(id)
            .expect("orphan store io")
            .expect("orphan record persisted for abandoned child");
        assert_eq!(record.agent_id, id);
    }

    // Let the runner finish in the background so the test does not
    // leak tasks; this is timing-only, not a correctness assertion.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(runner.invocation_count(), 3);
}
