//! Phase 7b `JoinPolicy::All`: batch of 3 children, the middle one fails;
//! the result vec preserves spawn order with `Ok / Err / Ok` entries
//! and failed children do NOT cancel their siblings.

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
use parking_lot::Mutex;

use crate::common::{open_test_orphan_store, open_test_store, parent_at};

/// Child runner whose per-prompt behaviour is configured up-front: the
/// `prompt` field of each [`ChildRunContext`] selects an action in the
/// `script` map (`Ok("done")` or `Err("boom")`).
struct ScriptedRunner {
    script: Mutex<std::collections::HashMap<String, Result<String, String>>>,
}

impl ScriptedRunner {
    fn new(script: std::collections::HashMap<String, Result<String, String>>) -> Self {
        Self {
            script: Mutex::new(script),
        }
    }
}

#[async_trait]
impl ChildRunner for ScriptedRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        // Keep the children alive briefly so the "siblings don't get
        // cancelled" assertion has a chance to observe their final
        // results.
        tokio::time::sleep(Duration::from_millis(40)).await;
        let outcome = self.script.lock().get(&ctx.prompt).cloned();
        let (final_message, exit) = match outcome {
            Some(Ok(text)) => (text, SubagentExit::Completed),
            Some(Err(err)) => (String::new(), SubagentExit::Failed { reason: err }),
            None => (
                String::new(),
                SubagentExit::Failed {
                    reason: "no script entry".to_string(),
                },
            ),
        };
        Ok(SubagentResult {
            child_agent_id: Some(ctx.preassigned_agent_id),
            final_message,
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit,
        })
    }
}

#[tokio::test]
async fn join_all_returns_per_child_results_in_spawn_order() {
    let parent = parent_at(AgentMode::Agent, 0);
    let (store, _store_keep) = open_test_store();
    let (orphans, _orphan_keep) = open_test_orphan_store();
    let registry = Arc::new(FleetRegistry::new());
    let quota = Arc::new(QuotaPool::new());
    let leases = Arc::new(ParentLeaseRegistry::new());
    let mut script = std::collections::HashMap::new();
    script.insert("first".to_string(), Ok("first-done".to_string()));
    script.insert("second".to_string(), Err("second boom".to_string()));
    script.insert("third".to_string(), Ok("third-done".to_string()));
    let runner = Arc::new(ScriptedRunner::new(script));

    let spawner = FleetSpawner::with_default_derivation(
        store,
        registry,
        quota,
        leases,
        orphans,
        runner,
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

    let batch = spawner
        .spawn_batch(
            vec![make("first"), make("second"), make("third")],
            JoinPolicy::All,
        )
        .await
        .expect("batch spawn ok");

    assert_eq!(batch.agent_ids.len(), 3);
    assert_eq!(batch.policy, JoinPolicy::All);

    match batch.join().await {
        BatchOutcome::All(results) => {
            assert_eq!(results.len(), 3, "one result per child");
            let first = results[0].as_ref().expect("first ok");
            assert!(matches!(first.exit, SubagentExit::Completed));
            assert_eq!(first.final_message, "first-done");

            let second = results[1]
                .as_ref()
                .expect("second is a child Result::Ok with Failed exit");
            assert!(matches!(second.exit, SubagentExit::Failed { .. }));

            let third = results[2].as_ref().expect("third ok");
            assert!(matches!(third.exit, SubagentExit::Completed));
            assert_eq!(third.final_message, "third-done");
        }
        other => panic!("expected BatchOutcome::All, got {other:?}"),
    }
}
