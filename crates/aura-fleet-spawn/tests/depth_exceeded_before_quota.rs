//! Phase 7a invariant: a too-deep spawn must NOT consume a quota
//! ticket. The plan §13 lineage rule requires the depth check
//! (inside `aura-agent-subagent::derive_subagent`) to run BEFORE
//! `aura-fleet-quota::QuotaPool::try_acquire` so a rejected spawn
//! never holds fleet-layer resources.
//!
//! The test wires a deliberately-shallow `max_depth = 1`
//! derivation, spawns from a parent at `depth = 1` (so the child
//! would have depth 2 > max), and asserts:
//!   - the spawn fails with `SpawnError::Derivation(DepthExceeded)`,
//!   - the quota pool issued ZERO tickets,
//!   - the child runner was NEVER invoked,
//!   - the registry remains empty.

mod common;

use std::sync::Arc;

use aura_agent_subagent::{
    DefaultDerivation, DerivationError, SubagentDerivationConfig, SubagentOverrides,
};
use aura_core_modes::{AgentMode, SpawnMode};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnError, SpawnRequest,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

#[tokio::test]
async fn depth_exceeded_does_not_consume_quota_ticket() {
    let parent = parent_at(AgentMode::Agent, 1);
    let (store, _keep) = open_test_store();
    let (orphans, _orphan_dir) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::new());

    let mut config = SubagentDerivationConfig::default_for_phase_6a();
    config.max_depth = 1;
    let derivation = Arc::new(DefaultDerivation::new(config));

    let spawner = FleetSpawner::new(
        store,
        registry.clone(),
        quota.clone(),
        leases.clone(),
        orphans,
        derivation,
        runner.clone(),
        FleetSpawnerConfig::default(),
    );

    let err = spawner
        .spawn(
            SpawnRequest {
                parent,
                overrides: SubagentOverrides::default(),
                prompt: "too deep".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            SpawnMode::Wait,
        )
        .await
        .expect_err("depth-exceeded must reject the spawn");

    match err {
        SpawnError::Derivation(DerivationError::DepthExceeded {
            parent_depth,
            max_depth,
        }) => {
            assert_eq!(parent_depth, 1);
            assert_eq!(max_depth, 1);
        }
        other => panic!("expected Derivation::DepthExceeded, got {other:?}"),
    }

    assert_eq!(
        quota.outstanding(),
        0,
        "quota MUST NOT issue a ticket for a depth-rejected spawn"
    );
    assert_eq!(
        runner.invocation_count(),
        0,
        "child runner MUST NOT run for a depth-rejected spawn"
    );
    assert!(
        registry.is_empty(),
        "registry MUST remain empty for a depth-rejected spawn"
    );
}
