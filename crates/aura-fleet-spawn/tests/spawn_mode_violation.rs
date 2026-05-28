//! Phase 7a: spawn requests originating from `Plan` / `Ask` /
//! `Debug` parents must be rejected with
//! [`ModeViolation::SpawnNotAllowed`] BEFORE any lease / quota /
//! audit work happens.
//!
//! Three tests — one per disallowed mode — assert:
//!   - the call returns `SpawnError::ModeViolation(SpawnNotAllowed)`,
//!   - the child runner was NEVER invoked,
//!   - the quota pool issued ZERO tickets.

mod common;

use std::sync::Arc;

use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, ModeViolation, SpawnMode};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    FleetSpawner, FleetSpawnerConfig, ParentLeaseRegistry, SpawnError, SpawnRequest,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at, FakeChildRunner};

async fn assert_mode_rejected(mode: AgentMode) {
    let parent = parent_at(mode, 0);
    let (store, _keep) = open_test_store();
    let (orphans, _orphan_dir) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(FakeChildRunner::new());
    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry.clone(),
        quota.clone(),
        leases.clone(),
        orphans,
        runner.clone(),
        FleetSpawnerConfig::default(),
    );

    let err = spawner
        .spawn(
            SpawnRequest {
                parent,
                overrides: SubagentOverrides::default(),
                prompt: "do thing".to_string(),
                originating_user_id: Some("user-root".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            SpawnMode::Wait,
        )
        .await
        .expect_err("spawn must reject disallowed mode");

    match err {
        SpawnError::ModeViolation(ModeViolation::SpawnNotAllowed) => {}
        other => panic!("expected ModeViolation::SpawnNotAllowed, got {other:?}"),
    }

    assert_eq!(runner.invocation_count(), 0, "runner must not be invoked");
    assert_eq!(quota.outstanding(), 0, "quota must not issue a ticket");
    assert!(
        registry.is_empty(),
        "registry must remain empty after rejection"
    );
    assert_eq!(
        leases.known_parents(),
        0,
        "no parent lease entry should be created for a fast-failed mode check"
    );
}

#[tokio::test]
async fn plan_mode_parent_spawn_rejected_with_mode_violation() {
    assert_mode_rejected(AgentMode::Plan).await;
}

#[tokio::test]
async fn ask_mode_parent_spawn_rejected_with_mode_violation() {
    assert_mode_rejected(AgentMode::Ask).await;
}

#[tokio::test]
async fn debug_mode_parent_spawn_rejected_with_mode_violation() {
    assert_mode_rejected(AgentMode::Debug).await;
}
