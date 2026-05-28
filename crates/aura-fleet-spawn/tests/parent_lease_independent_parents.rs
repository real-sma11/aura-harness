//! Phase 7a invariant: two parents spawning concurrently must run
//! in parallel — the lease-per-parent design replaces today's
//! coarse `spawn_lock` precisely so unrelated parents stop blocking
//! each other.
//!
//! The test wires a [`FakeChildRunner`] that sleeps 100ms per
//! invocation, kicks off two spawns from DISTINCT parents
//! concurrently, and asserts the wall-clock elapsed is well under
//! 150ms (i.e. NOT the 200ms a serial execution would take).

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
async fn two_distinct_parents_spawn_in_parallel() {
    let parent_a = parent_at(AgentMode::Agent, 0);
    let parent_b = parent_at(AgentMode::Agent, 0);
    assert_ne!(parent_a.agent_id, parent_b.agent_id);

    let (store, _keep) = open_test_store();
    let (orphans, _orphan_dir) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::with_delay(Duration::from_millis(100)));
    let spawner = Arc::new(FleetSpawner::with_default_derivation(
        store,
        registry.clone(),
        quota.clone(),
        leases.clone(),
        orphans,
        runner.clone(),
        FleetSpawnerConfig::default(),
    ));

    let start = Instant::now();
    let spawner_a = spawner.clone();
    let spawner_b = spawner;
    let p_a = parent_a;
    let p_b = parent_b;
    let handle_a = tokio::spawn(async move {
        spawner_a
            .spawn(
                SpawnRequest {
                    parent: p_a,
                    overrides: SubagentOverrides::default(),
                    prompt: "a".to_string(),
                    originating_user_id: None,
                    tool_call_id: None,
                    cancellation: None,
                },
                SpawnMode::Wait,
            )
            .await
    });
    let handle_b = tokio::spawn(async move {
        spawner_b
            .spawn(
                SpawnRequest {
                    parent: p_b,
                    overrides: SubagentOverrides::default(),
                    prompt: "b".to_string(),
                    originating_user_id: None,
                    tool_call_id: None,
                    cancellation: None,
                },
                SpawnMode::Wait,
            )
            .await
    });
    handle_a.await.unwrap().expect("parent A spawn ok");
    handle_b.await.unwrap().expect("parent B spawn ok");
    let elapsed = start.elapsed();

    // Parallel: ~100ms total. Allow a generous 150ms upper bound to
    // absorb runtime scheduling jitter without making the test flaky.
    assert!(
        elapsed < Duration::from_millis(150),
        "expected wall-clock parallelism (<150ms), got {elapsed:?}"
    );
    // Sanity-check both runners actually ran (cross-check against
    // the parallelism timing — if either runner was skipped the
    // wall-clock would also be tiny).
    assert_eq!(runner.invocation_count(), 2);
    assert_eq!(leases.known_parents(), 2);
}
