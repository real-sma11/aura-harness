//! Proves the architectural unification: a `task` subagent launches
//! through the shared child-kernel factory seam (not the scheduler's
//! bare node resolver), and the child's ancestor `parent_chain` is
//! propagated into the resolver build in production.
//!
//! When a [`aura_engine::child_kernel::ChildKernelFactory`] is injected
//! into the [`RuntimeChildRunner`], the runner MUST call
//! `build_child_router` exactly once per child run with the child's
//! narrowed permissions and its `parent_chain` (the lineage the depth /
//! cycle guards key off). The default (no-factory) path is still
//! covered by the sibling `task_tool_*` tests, which keep passing.

use std::sync::{Arc, Mutex};

use aura_agent_kernel::ExecutorRouter;
use aura_agent_subagent::SubagentRegistry;
use aura_core_types::{
    AgentId, AgentPermissions, AgentScope, Capability, SubagentDispatchRequest, UserToolDefaults,
};
use aura_engine::child_kernel::{ChildKernelFactory, ChildKernelRequest};
use aura_engine::child_runner::RuntimeChildRunner;
use aura_engine::scheduler::Scheduler;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{ChildRunner, OrphanStore, ParentLeaseRegistry};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_model_reasoner::MockProvider;
use aura_store_db::{RocksStore, Store};
use aura_tools::{SubagentDispatchHook, ToolCatalog};

/// Stub factory: records every [`ChildKernelRequest`] it is asked to
/// build a router for, then returns an empty router (the MockProvider
/// child loop emits plain text, so no executors are needed).
struct RecordingFactory {
    seen: Arc<Mutex<Vec<ChildKernelRequest>>>,
}

impl ChildKernelFactory for RecordingFactory {
    fn build_child_router(&self, request: ChildKernelRequest) -> ExecutorRouter {
        self.seen.lock().unwrap().push(request);
        ExecutorRouter::new()
    }
}

#[tokio::test]
async fn task_child_run_uses_injected_factory_with_parent_chain() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(dir.path().join("db"), false).expect("rocks open"));
    let provider = Arc::new(MockProvider::simple_response("child done"));
    let catalog = ToolCatalog::default();
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider,
        Vec::new(),
        catalog.executor_builtin_tools(),
        workspace.path().to_path_buf(),
        None,
    ));
    let registry = SubagentRegistry::bundled();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let factory: Arc<dyn ChildKernelFactory> = Arc::new(RecordingFactory { seen: seen.clone() });

    let orphan_dir = std::env::temp_dir().join("aura-test-orphans-factory");
    let child_runner: Arc<dyn ChildRunner> = Arc::new(
        RuntimeChildRunner::new(store.clone(), scheduler.clone(), registry.clone())
            .with_child_kernel_factory(factory),
    );
    let dispatch = FleetSubagentDispatcher::with_components(
        store.clone(),
        registry,
        Arc::new(FleetRegistry::new()),
        Arc::new(QuotaPool::new()),
        Arc::new(ParentLeaseRegistry::new()),
        Arc::new(OrphanStore::new(orphan_dir)),
        child_runner,
    );

    // Simulate what the `task` tool produces for a top-level parent:
    // `parent_chain == [parent_agent_id]` (the child's single ancestor).
    let parent_agent_id = AgentId::generate();
    let result = SubagentDispatchHook::dispatch(
        &dispatch,
        SubagentDispatchRequest {
            parent_agent_id,
            subagent_type: "explore".into(),
            prompt: "investigate".into(),
            originating_user_id: Some("factory-user".into()),
            parent_chain: vec![parent_agent_id],
            model_override: None,
            system_prompt_addendum: None,
            parent_permissions: AgentPermissions {
                scope: AgentScope::default(),
                capabilities: vec![Capability::SpawnAgent],
            },
            parent_tool_permissions: None,
            user_tool_defaults: UserToolDefaults::full_access(),
            tool_call_id: None,
            parent_mode: None,
            parent_kernel_mode: None,
            parent_model_id: None,
            override_mode: None,
            override_permissions: None,
            override_tool_subset: None,
            override_isolation_id: None,
            override_budget: None,
            spawn_mode: None,
            council_index: None,
            council_parent_tool_use_id: None,
        },
    )
    .await
    .expect("dispatch");

    assert!(
        result.child_agent_id.is_some(),
        "child run should mint a child agent id"
    );

    let recorded = seen.lock().unwrap();
    assert_eq!(
        recorded.len(),
        1,
        "the injected factory must build exactly one child router for the run"
    );
    let request = &recorded[0];
    assert_eq!(
        request.parent_chain,
        vec![parent_agent_id],
        "the child's ancestor parent_chain must be propagated into the resolver build"
    );
    assert_eq!(
        request.child_agent_id,
        result.child_agent_id.unwrap(),
        "the router is built for the same child agent id the run reports"
    );
    assert_eq!(request.originating_user_id.as_deref(), Some("factory-user"));
}
