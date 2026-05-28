//! Phase 7b parallelism: two children from DIFFERENT parents running
//! concurrently must complete in roughly the duration of a single
//! child, not the sum of both. Today's removed `spawn_lock` used to
//! serialise every spawn; the per-parent lease keeps the
//! intra-parent invariant while letting unrelated parents proceed in
//! parallel.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, SpawnMode};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnRequest};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrelated_parents_spawn_in_parallel_under_wait_mode() {
    let parent_a = parent_at(AgentMode::Agent, 0);
    let parent_b = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::with_delay(Duration::from_millis(200)));

    let spawner = Arc::new(FleetSpawner::with_default_derivation(
        store,
        registry,
        quota,
        leases,
        orphans,
        runner.clone(),
        FleetSpawnerConfig::default(),
    ));

    let a = spawner.clone();
    let b = spawner;
    let pa = parent_a;
    let pb = parent_b;

    let started = Instant::now();
    let handle_a = tokio::spawn(async move {
        a.spawn(
            SpawnRequest {
                parent: pa,
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
    let handle_b = tokio::spawn(async move {
        b.spawn(
            SpawnRequest {
                parent: pb,
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

    handle_a.await.unwrap().expect("parent_a child ok");
    handle_b.await.unwrap().expect("parent_b child ok");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(350),
        "two unrelated parents must run children in parallel (got {elapsed:?})"
    );
    assert_eq!(runner.invocation_count(), 2);
}
