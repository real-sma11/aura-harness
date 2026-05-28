//! Phase 7b: when a parent's task future is dropped mid-flight (the
//! moral equivalent of a parent panic) the in-flight Detached child
//! is observable in the orphan store and survives to completion.
//!
//! Phase B / Commit 3 / Step 3a moved this from `aura-runtime/tests/`
//! to `aura-fleet-subagent/tests/` because the fleet-layer surface
//! is the natural home for the test.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentOverrides};
use aura_core::{AgentId, SubagentExit, SubagentResult};
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode, SpawnMode};
use aura_core_permissions::{AgentScope, Capability, Permissions};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunContext, ChildRunError, ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore,
    ParentLeaseRegistry, SpawnHandle, SpawnRequest,
};
use aura_store::{RocksStore, Store};

struct SlowRunner;

#[async_trait]
impl ChildRunner for SlowRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(SubagentResult {
            child_agent_id: Some(ctx.preassigned_agent_id),
            final_message: "orphaned child finished".into(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit: SubagentExit::Completed,
        })
    }
}

fn parent() -> ParentContext {
    let agent_id = AgentId::generate();
    ParentContext {
        agent_id,
        depth: 0,
        mode: AgentMode::Agent,
        mode_profile: ModeProfile {
            agent: AgentMode::Agent,
            kernel: KernelMode::Audited,
            sandbox: SandboxMode::Standard,
            replay: ReplayMode::Live,
        },
        permissions: Permissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent],
        },
        model_id: "claude-opus-4-7".into(),
        lineage: SubagentLineage::from_root(agent_id),
    }
}

#[tokio::test]
async fn detached_child_survives_parent_drop_and_is_reapable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(temp.path().join("db"), false).expect("rocks open"));
    let orphan_dir = tempfile::tempdir().expect("orphan dir");
    let orphans = Arc::new(OrphanStore::new(orphan_dir.path().to_path_buf()));
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(SlowRunner);
    let spawner = Arc::new(FleetSpawner::with_default_derivation(
        store,
        registry.clone(),
        quota.clone(),
        leases.clone(),
        orphans.clone(),
        runner,
        FleetSpawnerConfig::default(),
    ));

    let parent = parent();
    let sp = spawner.clone();
    let orphan_id = {
        // Simulate a parent task that spawns a detached child and
        // then immediately drops its future before observing the
        // result.
        let handle = sp
            .spawn(
                SpawnRequest {
                    parent: parent.clone(),
                    overrides: SubagentOverrides::default(),
                    prompt: "orphan me".into(),
                    originating_user_id: Some("user-root".into()),
                    tool_call_id: None,
                    cancellation: None,
                },
                SpawnMode::Detached,
            )
            .await
            .expect("spawn detached");
        match handle {
            SpawnHandle::Detached(d) => d.agent_id,
            other => panic!("expected Detached, got {other:?}"),
        }
    };

    // Orphan record visible to `aura agents inspect`.
    let listed = orphans.list().expect("list orphans");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent_id, orphan_id);

    // Wait for the child to complete in the background.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(quota.outstanding(), 0, "quota released after child");

    // Simulate `aura agents reap <agent_id>` — remove the orphan
    // record from disk.
    orphans.remove(orphan_id).expect("reap");
    assert!(orphans.list().unwrap().is_empty());
}
