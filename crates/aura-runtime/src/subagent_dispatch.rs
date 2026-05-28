//! Foreground `task` subagent dispatch — Phase 7b routes through
//! the fleet layer with the full override surface.
//!
//! Phase 7b retires the Phase 7a `TaskCompatContext` shim: every
//! field the task path needs (`subagent_type`,
//! `system_prompt_addendum`, `parent_tool_permissions`,
//! `user_tool_defaults`, parent mode/kernel/model snapshot) is now
//! threaded through [`SubagentOverrides`] / [`SubagentSpec`] /
//! [`ChildRunContext`] directly. The runtime adapter still owns the
//! byte-identical translation from agent-loop outcome →
//! [`SubagentResult`] so the existing task tool surface remains
//! stable.

use crate::scheduler::Scheduler;
use crate::subagent_registry::SubagentRegistry;
use async_trait::async_trait;
use aura_agent::AgentLoopConfig;
use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentOverrides};
use aura_core::{
    resolve_effective_permission, AgentMode as CoreAgentMode, AgentPermissions,
    AgentToolPermissions, KernelMode as CoreKernelMode, SubagentDispatchRequest, SubagentExit,
    SubagentKindSpec, SubagentResult, ToolState, Transaction, TransactionType, UserDefaultMode,
    UserToolDefaults,
};
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode, SpawnMode};
use aura_core_permissions::Permissions;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunContext, ChildRunError, ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore,
    ParentLeaseRegistry, SpawnError, SpawnHandle, SpawnRequest,
};
use aura_kernel::PolicyConfig;
use aura_store::Store;
use aura_tools::SubagentDispatchHook;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Foreground `task` dispatcher backed by the fleet layer.
pub struct RuntimeSubagentDispatch {
    registry: SubagentRegistry,
    spawner: Arc<FleetSpawner>,
}

impl std::fmt::Debug for RuntimeSubagentDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSubagentDispatch")
            .field("registry_kinds", &self.registry.all().len())
            .finish()
    }
}

impl RuntimeSubagentDispatch {
    /// Construct a default [`RuntimeSubagentDispatch`] backed by a
    /// freshly-built [`FleetSpawner`] over `store` + `scheduler` +
    /// `child_runner`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, scheduler: Arc<Scheduler>) -> Self {
        let registry = SubagentRegistry::bundled();
        let orphan_dir = std::env::temp_dir().join("aura-test-orphans");
        Self::with_components(
            store.clone(),
            scheduler.clone(),
            registry.clone(),
            Arc::new(FleetRegistry::new()),
            Arc::new(QuotaPool::new()),
            Arc::new(ParentLeaseRegistry::new()),
            Arc::new(OrphanStore::new(orphan_dir)),
            Arc::new(RuntimeChildRunner::new(store, scheduler, registry)),
        )
    }

    /// Explicit constructor used by callers that already have a
    /// shared [`FleetRegistry`] / [`QuotaPool`] / [`ParentLeaseRegistry`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_components(
        store: Arc<dyn Store>,
        _scheduler: Arc<Scheduler>,
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
    /// custom [`SubagentKindSpec`] needs to be available in addition
    /// to the bundled defaults.
    #[must_use]
    pub fn with_registry(mut self, registry: SubagentRegistry) -> Self {
        self.registry = registry;
        self
    }
}

#[async_trait]
impl SubagentDispatchHook for RuntimeSubagentDispatch {
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
            Err(err) => Ok(spawn_error_to_subagent_result(err)),
        }
    }
}

/// Build the [`ParentContext`] consumed by
/// [`aura_agent_subagent::DefaultDerivation`].
///
/// Phase 7b honours every optional snapshot field on the request
/// (`parent_mode`, `parent_kernel_mode`, `parent_model_id`); when a
/// field is `None` the legacy Phase 7a defaults apply so old callers
/// continue to work.
fn parent_context_from_request(request: &SubagentDispatchRequest) -> ParentContext {
    let lineage = if request.parent_chain.is_empty() {
        SubagentLineage::from_root(request.parent_agent_id)
    } else {
        SubagentLineage {
            root_agent_id: request
                .parent_chain
                .last()
                .copied()
                .unwrap_or(request.parent_agent_id),
            chain: request.parent_chain.clone(),
        }
    };
    let permissions = legacy_permissions_to_modes(&request.parent_permissions);
    let depth = u32::try_from(request.parent_chain.len()).unwrap_or(u32::MAX);
    let mode = request
        .parent_mode
        .map_or(AgentMode::Agent, core_to_modes_mode);
    let kernel = request
        .parent_kernel_mode
        .map_or(KernelMode::Audited, core_to_modes_kernel);
    let model_id = request.parent_model_id.clone().unwrap_or_default();
    ParentContext {
        agent_id: request.parent_agent_id,
        depth,
        mode,
        mode_profile: ModeProfile {
            agent: mode,
            kernel,
            sandbox: SandboxMode::Standard,
            replay: ReplayMode::Live,
        },
        permissions,
        model_id,
        lineage,
    }
}

fn core_to_modes_mode(mode: CoreAgentMode) -> AgentMode {
    // `aura_core::AgentMode` is a re-export of `aura_core_modes::AgentMode`
    // so the conversion is a no-op clone — the two-step alias keeps
    // the call sites readable.
    match mode {
        CoreAgentMode::Agent => AgentMode::Agent,
        CoreAgentMode::Plan => AgentMode::Plan,
        CoreAgentMode::Ask => AgentMode::Ask,
        CoreAgentMode::Debug => AgentMode::Debug,
    }
}

fn core_to_modes_kernel(kernel: CoreKernelMode) -> KernelMode {
    match kernel {
        CoreKernelMode::Audited => KernelMode::Audited,
        CoreKernelMode::AuditedLite => KernelMode::AuditedLite,
    }
}

/// Build [`SubagentOverrides`] from the parent's request +
/// the resolved kind. Phase 7b absorbs the previously-out-of-band
/// fields directly into the overrides struct so the spawner no
/// longer needs a separate compat carrier.
fn overrides_from_request(
    request: &SubagentDispatchRequest,
    kind: &SubagentKindSpec,
) -> SubagentOverrides {
    let narrowed_parent = narrow_permissions(&request.parent_permissions, kind);
    let mode_override = request.override_mode.map(core_to_modes_mode);
    let tool_subset = request
        .override_tool_subset
        .clone()
        .unwrap_or_else(|| kind.allowed_tools.clone());
    let permissions = if let Some(explicit) = &request.override_permissions {
        // Honour the explicit override (subject to derivation's
        // narrowing-only rule); intersect with kind defaults so the
        // child still cannot widen past the kind's allow-list.
        let restricted = narrow_permissions(explicit, kind);
        Some(legacy_permissions_to_modes(&restricted))
    } else {
        Some(legacy_permissions_to_modes(&narrowed_parent))
    };
    let budget = request
        .override_budget
        .clone()
        .map(|b| aura_agent_subagent::SubagentBudget {
            max_tokens: b.max_tokens.unwrap_or(64_000),
            max_iterations: b.max_iterations,
            timeout_ms: b.timeout_ms,
        });
    SubagentOverrides {
        mode: mode_override,
        permissions,
        kernel_mode: None,
        model_id: request
            .model_override
            .clone()
            .or_else(|| kind.default_model.clone()),
        kind: Some(kind.name.clone()),
        spawn_mode: None,
        join_policy: None,
        replay_mode: None,
        budget,
        tool_subset: Some(tool_subset),
        isolation_id: request.override_isolation_id.clone(),
        subagent_type: Some(kind.name.clone()),
        system_prompt_addendum: request.system_prompt_addendum.clone(),
        parent_tool_permissions: request.parent_tool_permissions.clone(),
        user_tool_defaults: Some(request.user_tool_defaults.clone()),
    }
}

fn legacy_permissions_to_modes(legacy: &AgentPermissions) -> Permissions {
    Permissions {
        scope: legacy.scope.clone(),
        capabilities: legacy.capabilities.clone(),
    }
}

fn spawn_error_to_subagent_result(err: SpawnError) -> SubagentResult {
    SubagentResult::rejected(format!("spawn: {err}"))
}

/// [`ChildRunner`] implementation backed by the [`Scheduler`] +
/// [`Store`] pair.
pub struct RuntimeChildRunner {
    store: Arc<dyn Store>,
    scheduler: Arc<Scheduler>,
    registry: SubagentRegistry,
    spawn_hook: aura_kernel::KernelSpawnHook,
}

impl std::fmt::Debug for RuntimeChildRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeChildRunner")
            .field("registry_kinds", &self.registry.all().len())
            .finish()
    }
}

impl RuntimeChildRunner {
    /// Construct a [`RuntimeChildRunner`] over the supplied store /
    /// scheduler / registry.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        scheduler: Arc<Scheduler>,
        registry: SubagentRegistry,
    ) -> Self {
        Self {
            spawn_hook: aura_kernel::KernelSpawnHook::new(store.clone()),
            store,
            scheduler,
            registry,
        }
    }
}

#[async_trait]
impl ChildRunner for RuntimeChildRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        let originating_user_id = ctx.originating_user_id.clone();
        let subagent_type = ctx
            .spec
            .subagent_type
            .clone()
            .or_else(|| Some(ctx.spec.kind.clone()))
            .ok_or_else(|| {
                ChildRunError::Internal("ChildRunContext missing subagent_type".into())
            })?;

        let Some(kind) = self.registry.get(&subagent_type).cloned() else {
            return Ok(SubagentResult::rejected(format!(
                "unknown subagent type '{subagent_type}'"
            )));
        };

        let parent_permissions = ctx
            .spec
            .parent_tool_permissions
            .as_ref()
            .map(|_| ())
            .map_or_else(
                || AgentPermissions {
                    scope: ctx.spec.permissions.scope.clone(),
                    capabilities: ctx.spec.permissions.capabilities.clone(),
                },
                |_| AgentPermissions {
                    scope: ctx.spec.permissions.scope.clone(),
                    capabilities: ctx.spec.permissions.capabilities.clone(),
                },
            );
        let child_permissions = narrow_permissions(&parent_permissions, &kind);
        let user_defaults = ctx
            .spec
            .user_tool_defaults
            .clone()
            .unwrap_or_else(UserToolDefaults::full_access);
        let child_tool_permissions = narrowed_tool_permissions(
            ctx.spec.parent_tool_permissions.as_ref(),
            &user_defaults,
            &kind,
        );
        let preassigned_id = ctx.preassigned_agent_id;
        let child_spec = aura_kernel::ChildAgentSpec {
            name: kind.name.clone(),
            role: format!("subagent:{}", kind.name),
            permissions: child_permissions.clone(),
            tool_permissions: Some(child_tool_permissions.clone()),
            parent_tool_permissions: ctx.spec.parent_tool_permissions.clone(),
            system_prompt_override: Some(system_prompt_for(
                &kind,
                ctx.spec.system_prompt_addendum.as_deref(),
            )),
            preassigned_agent_id: Some(preassigned_id),
        };

        let spawn_outcome = {
            use aura_kernel::SpawnHook;
            self.spawn_hook
                .spawn_child(
                    &ctx.parent_agent_id,
                    originating_user_id.as_deref(),
                    child_spec,
                )
                .await
                .map_err(|e| ChildRunError::Internal(format!("spawn child: {e}")))?
        };
        let child_agent_id = spawn_outcome.child_agent_id;

        let tx = Transaction::new_chained(
            child_agent_id,
            TransactionType::UserPrompt,
            Bytes::from(ctx.prompt.clone().into_bytes()),
            None,
        );
        self.store
            .enqueue_tx(&tx)
            .map_err(|e| ChildRunError::Internal(format!("enqueue child prompt: {e}")))?;

        let child_model = ctx.spec.model_id.as_str().to_string();
        let child_model = if child_model.is_empty() {
            kind.default_model
                .clone()
                .unwrap_or_else(|| aura_reasoner::ENV_FALLBACK_MODEL.to_string())
        } else {
            child_model
        };
        if let Some(parent) = self.scheduler.identity_registry().get(ctx.parent_agent_id) {
            let mut child_identity = parent.clone();
            child_identity.model = child_model.clone();
            child_identity.system_prompt =
                system_prompt_for(&kind, ctx.spec.system_prompt_addendum.as_deref());
            self.scheduler
                .identity_registry()
                .register(child_agent_id, child_identity);
        }
        let loop_config = loop_config_for(&kind, &child_model);
        let policy = policy_for(child_permissions, child_tool_permissions, &user_defaults);
        if kind.budget.timeout_ms == 0 {
            return Ok(SubagentResult {
                child_agent_id: Some(child_agent_id),
                final_message: String::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Timeout,
            });
        }
        let processed = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                return Ok(SubagentResult {
                    child_agent_id: Some(child_agent_id),
                    final_message: String::new(),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    files_changed: Vec::new(),
                    exit: SubagentExit::Cancelled,
                });
            }
            outcome = tokio::time::timeout(
                Duration::from_millis(kind.budget.timeout_ms),
                self.scheduler.schedule_agent_with_overrides(
                    child_agent_id,
                    Some(loop_config),
                    Some(policy),
                ),
            ) => match outcome {
                Ok(result) => result.map_err(|e| ChildRunError::Internal(format!("schedule: {e}")))?,
                Err(_) => {
                    return Ok(SubagentResult {
                        child_agent_id: Some(child_agent_id),
                        final_message: String::new(),
                        total_input_tokens: 0,
                        total_output_tokens: 0,
                        files_changed: Vec::new(),
                        exit: SubagentExit::Timeout,
                    });
                }
            }
        };

        let Some(result) = processed.last_result else {
            warn!(
                child_agent_id = %child_agent_id,
                "fleet child runner: agent loop produced no result"
            );
            return Ok(SubagentResult {
                child_agent_id: Some(child_agent_id),
                final_message: String::new(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Failed {
                    reason: "child processed no agent loop result".into(),
                },
            });
        };

        let exit = result
            .llm_error
            .as_ref()
            .map_or(SubagentExit::Completed, |reason| SubagentExit::Failed {
                reason: reason.clone(),
            });
        Ok(SubagentResult {
            child_agent_id: Some(child_agent_id),
            final_message: result.total_text,
            total_input_tokens: result.total_input_tokens,
            total_output_tokens: result.total_output_tokens,
            files_changed: result
                .file_changes
                .into_iter()
                .map(|change| change.path)
                .collect(),
            exit,
        })
    }
}

fn narrow_permissions(parent: &AgentPermissions, kind: &SubagentKindSpec) -> AgentPermissions {
    let capabilities = parent
        .capabilities
        .iter()
        .filter(|held| {
            kind.allowed_capabilities
                .iter()
                .any(|allowed| held.satisfies(allowed))
        })
        .cloned()
        .collect();
    AgentPermissions {
        scope: parent.scope.clone(),
        capabilities,
    }
}

fn narrowed_tool_permissions(
    parent_tool_permissions: Option<&AgentToolPermissions>,
    user_tool_defaults: &UserToolDefaults,
    kind: &SubagentKindSpec,
) -> AgentToolPermissions {
    let mut per_tool = BTreeMap::new();
    for tool in &kind.allowed_tools {
        let parent_state =
            resolve_effective_permission(user_tool_defaults, parent_tool_permissions, tool);
        per_tool.insert(tool.clone(), parent_state);
    }
    AgentToolPermissions { per_tool }
}

fn policy_for(
    permissions: AgentPermissions,
    tool_permissions: AgentToolPermissions,
    user_tool_defaults: &UserToolDefaults,
) -> PolicyConfig {
    let fallback = match user_tool_defaults.mode {
        UserDefaultMode::AutoReview => ToolState::Ask,
        _ => ToolState::Deny,
    };
    let user_default = UserToolDefaults::default_permissions(BTreeMap::new(), fallback);
    PolicyConfig::default()
        .with_agent_permissions(permissions)
        .with_user_default(user_default)
        .with_agent_override(Some(tool_permissions))
}

fn loop_config_for(kind: &SubagentKindSpec, model: &str) -> AgentLoopConfig {
    let mut config = AgentLoopConfig {
        system_prompt: kind.system_prompt.clone(),
        max_iterations: kind.budget.max_iterations as usize,
        ..AgentLoopConfig::for_agent(model)
    };
    if let Some(max_tokens) = kind.budget.max_tokens {
        config.max_tokens = max_tokens;
    }
    config
}

fn system_prompt_for(kind: &SubagentKindSpec, addendum: Option<&str>) -> String {
    match addendum {
        Some(addendum) if !addendum.trim().is_empty() => {
            format!("{}\n\n{}", kind.system_prompt, addendum)
        }
        _ => kind.system_prompt.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use aura_core::{AgentId, AgentScope, Capability};
    use aura_reasoner::MockProvider;
    use aura_store::{ReadStore, RocksStore};
    use aura_tools::ToolCatalog;

    #[test]
    fn narrow_permissions_keeps_only_kind_allowed_caps() {
        let parent = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent, Capability::ReadAgent],
        };
        let mut kind = SubagentRegistry::bundled()
            .get("general_purpose")
            .unwrap()
            .clone();
        kind.allowed_capabilities = vec![Capability::ReadAgent];
        let narrowed = narrow_permissions(&parent, &kind);
        assert_eq!(narrowed.capabilities, vec![Capability::ReadAgent]);
    }

    #[test]
    fn policy_fallback_denies_tools_outside_allowlist() {
        let user = UserToolDefaults::full_access();
        let registry = SubagentRegistry::bundled();
        let kind = registry.get("explore").unwrap();
        let tool_permissions = narrowed_tool_permissions(None, &user, kind);
        let policy = policy_for(AgentPermissions::empty(), tool_permissions, &user);
        assert_eq!(
            resolve_effective_permission(
                &policy.user_default,
                policy.agent_override.as_ref(),
                "write_file",
            ),
            ToolState::Deny
        );
        assert_eq!(
            resolve_effective_permission(
                &policy.user_default,
                policy.agent_override.as_ref(),
                "read_file",
            ),
            ToolState::Allow
        );
    }

    fn dispatch_request(parent_agent_id: AgentId) -> SubagentDispatchRequest {
        SubagentDispatchRequest {
            parent_agent_id,
            subagent_type: "explore".into(),
            prompt: "summarize".into(),
            originating_user_id: Some("user".into()),
            parent_chain: Vec::new(),
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
        }
    }

    #[tokio::test]
    async fn dispatch_runs_child_and_records_parent_and_child_logs() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
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
        let dispatch = RuntimeSubagentDispatch::new(store.clone(), scheduler);
        let parent_agent_id = AgentId::generate();

        let result = dispatch
            .dispatch(dispatch_request(parent_agent_id))
            .await
            .unwrap();

        assert!(matches!(result.exit, SubagentExit::Completed));
        assert_eq!(result.final_message, "child done");
        let child_id = result.child_agent_id.expect("child id");
        assert!(
            !store
                .scan_record(parent_agent_id, 1, 10)
                .unwrap()
                .is_empty(),
            "spawn should record parent delegation"
        );
        let child_entries = store.scan_record(child_id, 1, 10).unwrap();
        assert!(
            child_entries
                .iter()
                .any(|entry| entry.tx.tx_type == TransactionType::AgentMsg),
            "child should record final assistant message"
        );
    }

    #[tokio::test]
    async fn dispatch_returns_timeout_for_exhausted_budget() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let provider = Arc::new(MockProvider::simple_response("late"));
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
        let mut kind = registry.get("explore").unwrap().clone();
        kind.name = "instant_timeout".into();
        kind.budget.timeout_ms = 0;
        let registry = SubagentRegistry::from_specs(vec![kind]);
        let store_for_runner = store.clone();
        let scheduler_for_runner = scheduler.clone();
        let runner = Arc::new(RuntimeChildRunner::new(
            store_for_runner,
            scheduler_for_runner,
            registry.clone(),
        ));
        let orphans = Arc::new(OrphanStore::new(
            std::env::temp_dir().join("aura-test-orphans-timeout"),
        ));
        let dispatch = RuntimeSubagentDispatch::with_components(
            store,
            scheduler,
            registry,
            Arc::new(FleetRegistry::new()),
            Arc::new(QuotaPool::new()),
            Arc::new(ParentLeaseRegistry::new()),
            orphans,
            runner,
        );

        let mut req = dispatch_request(AgentId::generate());
        req.subagent_type = "instant_timeout".into();
        let result = dispatch.dispatch(req).await.unwrap();
        assert!(matches!(result.exit, SubagentExit::Timeout));
    }
}
