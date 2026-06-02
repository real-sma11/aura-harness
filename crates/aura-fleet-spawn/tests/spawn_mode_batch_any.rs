//! Phase 7b `JoinPolicy::Any`: batch of 3 children running with
//! staggered sleeps; the first to finish (the 50ms child) cancels the
//! siblings, so the call returns in ≈ 50ms + grace instead of the
//! 500ms required to await every sibling.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_subagent::SubagentOverrides;
use aura_core_modes::{AgentMode, JoinPolicy};
use aura_core_types::{SubagentExit, SubagentResult};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    BatchOutcome, ChildRunContext, ChildRunError, ChildRunner, FleetSpawner, FleetSpawnerConfig,
    ParentLeaseRegistry, SpawnRequest,
};

use crate::common::{open_test_orphan_store, open_test_store, parent_at};

/// Child runner: each child sleeps for a duration parsed from the
/// prompt (`"sleep:50"` → 50ms) and returns success.
struct SleepRunner;

#[async_trait]
impl ChildRunner for SleepRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        let ms: u64 = ctx
            .prompt
            .strip_prefix("sleep:")
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);
        let result = tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(ms)) => SubagentResult {
                child_agent_id: Some(ctx.preassigned_agent_id),
                final_message: format!("done-{ms}"),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Completed,
            },
            () = ctx.cancellation.cancelled() => SubagentResult {
                child_agent_id: Some(ctx.preassigned_agent_id),
                final_message: String::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Cancelled,
            },
        };
        Ok(result)
    }
}

#[tokio::test]
async fn join_any_returns_first_success_and_cancels_siblings() {
    let parent = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let runner = Arc::new(SleepRunner);

    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry,
        quota,
        leases,
        orphans,
        runner,
        FleetSpawnerConfig::default(),
    );

    let make = |sleep_ms: u64| SpawnRequest {
        parent: parent.clone(),
        overrides: SubagentOverrides::default(),
        prompt: format!("sleep:{sleep_ms}"),
        originating_user_id: Some("user".to_string()),
        tool_call_id: None,
        cancellation: None,
    };

    let started = std::time::Instant::now();
    let batch = spawner
        .spawn_batch(vec![make(50), make(500), make(500)], JoinPolicy::Any)
        .await
        .expect("batch ok");
    let outcome = batch.join().await;
    let elapsed = started.elapsed();

    match outcome {
        BatchOutcome::Any(Ok(result)) => {
            assert_eq!(result.final_message, "done-50");
            assert!(matches!(result.exit, SubagentExit::Completed));
        }
        other => panic!("expected Any(Ok), got {other:?}"),
    }
    // Bounded by the fastest child + small overhead. The 500ms
    // siblings would dominate without proper cancellation.
    assert!(
        elapsed < Duration::from_millis(300),
        "Any-policy join must return shortly after the fastest child (got {elapsed:?})"
    );
}
