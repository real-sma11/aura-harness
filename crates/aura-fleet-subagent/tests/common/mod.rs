//! Test helpers for `aura-fleet-subagent` integration tests.
//!
//! Factor out the `FleetSubagentDispatcher` + `RuntimeChildRunner`
//! wiring that every `task_tool_*` test needs so the call sites stay
//! focused on the override field under test.

#![allow(dead_code)]

use aura_agent_subagent::SubagentRegistry;
use aura_engine::child_runner::RuntimeChildRunner;
use aura_engine::scheduler::Scheduler;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{ChildRunner, OrphanStore, ParentLeaseRegistry};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_reasoner::MockProvider;
use aura_store::{RocksStore, Store};
use aura_tools::ToolCatalog;
use std::sync::Arc;

/// Build a [`FleetSubagentDispatcher`] backed by a `RocksStore` +
/// in-process `Scheduler` + bundled [`SubagentRegistry`]. Mirrors the
/// pre-refactor `RuntimeSubagentDispatch::new` convenience shape so
/// the moved tests read line-for-line against the original.
pub fn build_dispatch_with_response(
    response: &'static str,
) -> (
    FleetSubagentDispatcher,
    Arc<dyn Store>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(dir.path().join("db"), false).expect("rocks open"));
    let provider = Arc::new(MockProvider::simple_response(response));
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
    let orphan_dir = std::env::temp_dir().join("aura-test-orphans");
    let child_runner: Arc<dyn ChildRunner> = Arc::new(RuntimeChildRunner::new(
        store.clone(),
        scheduler.clone(),
        registry.clone(),
    ));
    let dispatch = FleetSubagentDispatcher::with_components(
        store.clone(),
        registry,
        Arc::new(FleetRegistry::new()),
        Arc::new(QuotaPool::new()),
        Arc::new(ParentLeaseRegistry::new()),
        Arc::new(OrphanStore::new(orphan_dir)),
        child_runner,
    );
    (dispatch, store, dir, workspace)
}
