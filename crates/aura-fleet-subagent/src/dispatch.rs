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
use aura_agent_loop::AgentLoopEvent;
use aura_agent_subagent::{overrides_from_request, parent_context_from_request, SubagentRegistry};
use aura_core_types::{SubagentDispatchRequest, SubagentResult};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore, ParentLeaseRegistry, SpawnError,
    SpawnHandle, SpawnMode, SpawnRequest,
};
use aura_tools::SubagentDispatchHook;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
        store: Arc<dyn aura_store_db::Store>,
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
    /// custom [`aura_core_types::SubagentKindSpec`] needs to be available in
    /// addition to the bundled defaults.
    #[must_use]
    pub fn with_registry(mut self, registry: SubagentRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Dispatch a subagent run, optionally threading a streaming
    /// [`AgentLoopEvent`] sink and a caller [`CancellationToken`].
    ///
    /// This is the observability-aware superset of the
    /// [`SubagentDispatchHook::dispatch`] entry point. It honors
    /// `request.spawn_mode` (defaulting to [`SpawnMode::Wait`]):
    ///
    /// - `Wait` (default): block until the child completes and return
    ///   its [`SubagentResult`] inline — byte-identical to the legacy
    ///   blocking behavior.
    /// - `Detached`: spawn the child in the background and return an
    ///   immediate ack `SubagentResult` carrying the child agent id so
    ///   the parent can keep working while observing the child's live
    ///   thread.
    ///
    /// `event_tx` is forwarded to the child runner so the child loop
    /// streams events to an external observer (the WS client attached
    /// to the minted child run id). `cancellation` is forked into the
    /// child token so cancelling the parent turn propagates into a
    /// `Wait` child.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` only on infrastructure failures that
    /// cannot be represented as a `SubagentResult`. Rejections and
    /// child failures are returned inside an `Ok(SubagentResult)`.
    pub async fn spawn_with_events(
        &self,
        request: SubagentDispatchRequest,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancellation: Option<CancellationToken>,
    ) -> Result<SubagentResult, String> {
        let Some(kind) = self.registry.get(&request.subagent_type).cloned() else {
            return Ok(SubagentResult::rejected(format!(
                "unknown subagent type '{}'",
                request.subagent_type
            )));
        };

        let parent_ctx = parent_context_from_request(&request);
        let overrides = overrides_from_request(&request, &kind);
        let mode = request.spawn_mode.unwrap_or(SpawnMode::Wait);

        let spawn_request = SpawnRequest {
            parent: parent_ctx,
            overrides,
            prompt: request.prompt.clone(),
            originating_user_id: request.originating_user_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            cancellation,
        };

        match self
            .spawner
            .spawn_with_events(spawn_request, mode, event_tx)
            .await
        {
            Ok(SpawnHandle::Completed(result)) => Ok(result),
            Ok(SpawnHandle::Detached(detached)) => Ok(SubagentResult::completed(
                detached.agent_id,
                format!(
                    "subagent dispatched (detached); child agent {} running in background",
                    detached.agent_id
                ),
            )),
            Ok(other) => Ok(SubagentResult::rejected(format!(
                "task dispatch: unexpected handle variant {:?} for SpawnMode::{mode:?}",
                other.mode()
            ))),
            Err(err) => Ok(spawn_error_to_subagent_result(&err)),
        }
    }
}

#[async_trait]
impl SubagentDispatchHook for FleetSubagentDispatcher {
    async fn dispatch(&self, request: SubagentDispatchRequest) -> Result<SubagentResult, String> {
        // Default trait path: no streaming sink, no caller cancellation
        // — preserves the original blocking `Wait` dispatch exactly.
        self.spawn_with_events(request, None, None).await
    }
}

/// Translate a [`SpawnError`] into a parent-visible [`SubagentResult`]
/// tagged `Rejected`. Kept in the fleet-layer crate so the agent
/// layer never has to import `SpawnError`.
#[must_use]
pub fn spawn_error_to_subagent_result(err: &SpawnError) -> SubagentResult {
    SubagentResult::rejected(format!("spawn: {err}"))
}
