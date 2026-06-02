//! Phase 7b `SpawnMode::Detached`: the parent gets a `SpawnHandle::Detached`
//! immediately, the child runs to completion in a background tokio task,
//! and an orphan record is persisted under the configured root.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, SpawnMode};
use aura_core_types::SubagentExit;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnHandle, SpawnRequest,
};
use tokio_util::sync::CancellationToken;

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test]
async fn detached_spawn_returns_immediately_and_completes_in_background() {
    let parent = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::with_delay(Duration::from_millis(100)));

    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry.clone(),
        quota,
        leases,
        orphans.clone(),
        runner.clone(),
        FleetSpawnerConfig::default(),
    );

    let start = std::time::Instant::now();
    let handle = spawner
        .spawn(
            SpawnRequest {
                parent,
                overrides: SubagentOverrides::default(),
                prompt: "detached-child".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            SpawnMode::Detached,
        )
        .await
        .expect("detached spawn ok");
    let returned_in = start.elapsed();

    assert!(
        returned_in < Duration::from_millis(80),
        "detached spawn must return before child completes (got {returned_in:?})"
    );

    let SpawnHandle::Detached(detached) = handle else {
        panic!("expected SpawnHandle::Detached");
    };

    let child_id = detached.agent_id;
    let orphan_record = orphans
        .load(child_id)
        .expect("orphan store io")
        .expect("orphan record exists for detached child");
    assert_eq!(orphan_record.spawn_mode, SpawnMode::Detached);
    assert_eq!(orphan_record.mode, AgentMode::Agent);

    let result = detached.join().await.expect("child sent result");
    assert!(matches!(
        result.exit,
        aura_core_types::SubagentExit::Completed
    ));
    assert_eq!(result.child_agent_id, Some(child_id));
    assert_eq!(runner.invocation_count(), 1);
}

#[tokio::test]
async fn detached_spawn_ignores_parent_cancellation() {
    let parent = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::with_delay(Duration::from_millis(40)));
    let cancellation = CancellationToken::new();

    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry,
        quota,
        leases,
        orphans,
        runner,
        FleetSpawnerConfig::default(),
    );

    let handle = spawner
        .spawn(
            SpawnRequest {
                parent,
                overrides: SubagentOverrides::default(),
                prompt: "detached-child".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: Some(cancellation.clone()),
            },
            SpawnMode::Detached,
        )
        .await
        .expect("detached spawn ok");
    cancellation.cancel();

    let SpawnHandle::Detached(detached) = handle else {
        panic!("expected SpawnHandle::Detached");
    };
    let result = detached.join().await.expect("child sent result");
    assert!(
        matches!(result.exit, SubagentExit::Completed),
        "detached children must survive parent cancellation"
    );
}
