//! Foreground `task` subagent dispatcher backed by the fleet layer.
//!
//! Phase B / Commit 3 / Step 3a renamed `RuntimeSubagentDispatch`
//! (formerly in `aura-runtime/src/subagent_dispatch.rs`) to
//! [`FleetSubagentDispatcher`]. The dispatcher now lives at the fleet
//! layer because that's where it actually belongs in the dependency
//! graph: the concrete impl combines [`aura_fleet_spawn::FleetSpawner`]
//! with the agent-layer [`aura_agent_subagent::SubagentRegistry`] and
//! exposes the [`aura_tools::SubagentDispatchHook`] trait the `task`
//! tool consumes.
//!
//! Phase 7b retired the Phase 7a `TaskCompatContext` shim: every
//! field the task path needs (`subagent_type`,
//! `system_prompt_addendum`, `parent_tool_permissions`,
//! `user_tool_defaults`, parent mode/kernel/model snapshot) is now
//! threaded through `SubagentOverrides` / `SubagentSpec` /
//! `ChildRunContext` directly. The fleet-layer adapter still owns the
//! byte-identical translation from agent-loop outcome →
//! `SubagentResult` so the existing task tool surface remains stable.

use async_trait::async_trait;
use aura_agent_subagent::{overrides_from_request, parent_context_from_request, SubagentRegistry};
use aura_core::{SubagentDispatchRequest, SubagentResult};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore, ParentLeaseRegistry, SpawnError,
    SpawnHandle, SpawnMode, SpawnRequest,
};
use aura_tools::SubagentDispatchHook;
use std::sync::Arc;

/// Foreground `task` dispatcher backed by the fleet layer.
///
/// Wraps a [`FleetSpawner`] + [`SubagentRegistry`] pair. The composition
/// root in `aura-runtime::node` constructs one of these and hands it
/// to the `task` tool via the `Arc<dyn SubagentDispatchHook>` slot.
pub struct FleetSubagentDispatcher {
    registry: SubagentRegistry,
    spawner: Arc<FleetSpawner>,
}

impl std::fmt::Debug for FleetSubagentDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetSubagentDispatcher")
            .field("registry_kinds", &self.registry.all().len())
            .finish()
    }
}

impl FleetSubagentDispatcher {
    /// Explicit constructor used by callers that already have a
    /// shared [`FleetRegistry`] / [`QuotaPool`] / [`ParentLeaseRegistry`] /
    /// [`OrphanStore`] and an injected [`ChildRunner`].
    #[must_use]
    pub fn with_components(
        store: Arc<dyn aura_store::Store>,
        registry: SubagentRegistry,
        fleet_registry: Arc<FleetRegistry>,
        quota: Arc<QuotaPool>,
        leases: Arc<ParentLeaseRegistry>,
        orphans: Arc<OrphanStore>,
        child_runner: Arc<dyn ChildRunner>,
    ) -> Self {
        let spawner = Arc::new(FleetSpawner::with_default_derivation(
            store,
            fleet_registry,
            quota,
            leases,
            orphans,
            child_runner,
            FleetSpawnerConfig::default(),
        ));
        Self { registry, spawner }
    }

    /// Override the bundled subagent registry. Used in tests where a
    /// custom [`aura_core::SubagentKindSpec`] needs to be available in
    /// addition to the bundled defaults.
    #[must_use]
    pub fn with_registry(mut self, registry: SubagentRegistry) -> Self {
        self.registry = registry;
        self
    }
}

#[async_trait]
impl SubagentDispatchHook for FleetSubagentDispatcher {
    async fn dispatch(&self, request: SubagentDispatchRequest) -> Result<SubagentResult, String> {
        let Some(kind) = self.registry.get(&request.subagent_type).cloned() else {
            return Ok(SubagentResult::rejected(format!(
                "unknown subagent type '{}'",
                request.subagent_type
            )));
        };

        let parent_ctx = parent_context_from_request(&request);
        let overrides = overrides_from_request(&request, &kind);

        let spawn_request = SpawnRequest {
            parent: parent_ctx,
            overrides,
            prompt: request.prompt.clone(),
            originating_user_id: request.originating_user_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            cancellation: None,
        };

        match self.spawner.spawn(spawn_request, SpawnMode::Wait).await {
            Ok(SpawnHandle::Completed(result)) => Ok(result),
            Ok(other) => Ok(SubagentResult::rejected(format!(
                "task dispatch: unexpected handle variant {:?} for SpawnMode::Wait",
                other.mode()
            ))),
            Err(err) => Ok(spawn_error_to_subagent_result(&err)),
        }
    }
}

/// Translate a [`SpawnError`] into a parent-visible [`SubagentResult`]
/// tagged `Rejected`. Kept in the fleet-layer crate so the agent
/// layer never has to import `SpawnError`.
#[must_use]
pub fn spawn_error_to_subagent_result(err: &SpawnError) -> SubagentResult {
    SubagentResult::rejected(format!("spawn: {err}"))
}
