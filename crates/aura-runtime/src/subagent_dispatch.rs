//! Runtime implementation of foreground subagent dispatch.

use crate::scheduler::Scheduler;
use crate::subagent_registry::SubagentRegistry;
use async_trait::async_trait;
use aura_agent::AgentLoopConfig;
use aura_core::{
    resolve_effective_permission, AgentPermissions, AgentToolPermissions, SubagentDispatchRequest,
    SubagentExit, SubagentKindSpec, SubagentResult, ToolState, Transaction, TransactionType,
    UserDefaultMode, UserToolDefaults,
};
use aura_kernel::{ChildAgentSpec, KernelSpawnHook, PolicyConfig, SpawnHook};
use aura_store::Store;
use aura_tools::SubagentDispatchHook;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Foreground `task` dispatcher backed by the local scheduler.
pub struct RuntimeSubagentDispatch {
    store: Arc<dyn Store>,
    scheduler: Arc<Scheduler>,
    registry: SubagentRegistry,
    spawn_hook: KernelSpawnHook,
    /// Serializes `KernelSpawnHook` parent delegate writes while the current
    /// parent kernel is still inside a batch tool execution.
    spawn_lock: Mutex<()>,
}

impl RuntimeSubagentDispatch {
    #[must_use]
    pub fn new(store: Arc<dyn Store>, scheduler: Arc<Scheduler>) -> Self {
        Self {
            spawn_hook: KernelSpawnHook::new(store.clone()),
            store,
            scheduler,
            registry: SubagentRegistry::bundled(),
            spawn_lock: Mutex::new(()),
        }
    }

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

        let child_permissions = narrow_permissions(&request.parent_permissions, &kind);
        let child_tool_permissions = narrowed_tool_permissions(&request, &kind);
        let child_spec = ChildAgentSpec {
            name: kind.name.clone(),
            role: format!("subagent:{}", kind.name),
            permissions: child_permissions.clone(),
            tool_permissions: Some(child_tool_permissions.clone()),
            parent_tool_permissions: request.parent_tool_permissions.clone(),
            system_prompt_override: Some(system_prompt_for(&kind, &request)),
            preassigned_agent_id: None,
        };

        let spawn_outcome = {
            let _guard = self.spawn_lock.lock().await;
            self.spawn_hook
                .spawn_child(
                    &request.parent_agent_id,
                    request.originating_user_id.as_deref(),
                    child_spec,
                )
                .await
                .map_err(|e| format!("spawn child: {e}"))?
        };
        let child_agent_id = spawn_outcome.child_agent_id;

        let tx = Transaction::new_chained(
            child_agent_id,
            TransactionType::UserPrompt,
            Bytes::from(request.prompt.clone().into_bytes()),
            None,
        );
        self.store
            .enqueue_tx(&tx)
            .map_err(|e| format!("enqueue child prompt: {e}"))?;

        // Resolve the child's model in priority order: per-request
        // override, kind default, then the env-fallback identifier.
        // Once Step 2's [`AgentIdentityRegistry`] lands, the child's
        // identity is registered before
        // `schedule_agent_with_overrides` runs and the scheduler
        // applies it directly; the override path here remains the
        // explicit caller-driven shape (the parent passes a
        // `model_override` from a `task` tool call).
        let child_model = request
            .model_override
            .as_deref()
            .or(kind.default_model.as_deref())
            .unwrap_or(aura_reasoner::ENV_FALLBACK_MODEL)
            .to_string();
        // Register the child agent's identity in the
        // [`crate::scheduler::AgentIdentityRegistry`]. The dispatch
        // itself supplies an explicit `agent_loop_config` override
        // and bypasses the registry lookup; the registration here
        // covers the post-dispatch scenario where the child's
        // pending transactions are drained by a worker fan-out
        // (e.g. tool-permission update) that calls
        // `Scheduler::schedule_agent` without an override.
        if let Some(parent) = self
            .scheduler
            .identity_registry()
            .get(request.parent_agent_id)
        {
            let mut child_identity = parent.clone();
            child_identity.model = child_model.clone();
            child_identity.system_prompt = system_prompt_for(&kind, &request);
            self.scheduler
                .identity_registry()
                .register(child_agent_id, child_identity);
        }
        let loop_config = loop_config_for(&kind, &child_model);
        let policy = policy_for(child_permissions, child_tool_permissions, &request);
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
        let processed = match tokio::time::timeout(
            Duration::from_millis(kind.budget.timeout_ms),
            self.scheduler.schedule_agent_with_overrides(
                child_agent_id,
                Some(loop_config),
                Some(policy),
            ),
        )
        .await
        {
            Ok(result) => result.map_err(|e| format!("schedule child: {e}"))?,
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
        };

        let Some(result) = processed.last_result else {
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
    request: &SubagentDispatchRequest,
    kind: &SubagentKindSpec,
) -> AgentToolPermissions {
    let mut per_tool = BTreeMap::new();
    for tool in &kind.allowed_tools {
        let parent_state = resolve_effective_permission(
            &request.user_tool_defaults,
            request.parent_tool_permissions.as_ref(),
            tool,
        );
        per_tool.insert(tool.clone(), parent_state);
    }
    AgentToolPermissions { per_tool }
}

fn policy_for(
    permissions: AgentPermissions,
    tool_permissions: AgentToolPermissions,
    request: &SubagentDispatchRequest,
) -> PolicyConfig {
    let fallback = match request.user_tool_defaults.mode {
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

fn system_prompt_for(kind: &SubagentKindSpec, request: &SubagentDispatchRequest) -> String {
    match request.system_prompt_addendum.as_deref() {
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
        let request = SubagentDispatchRequest {
            parent_agent_id: AgentId::generate(),
            subagent_type: "explore".into(),
            prompt: "inspect".into(),
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
        };
        let registry = SubagentRegistry::bundled();
        let kind = registry.get("explore").unwrap();
        let tool_permissions = narrowed_tool_permissions(&request, kind);
        let policy = policy_for(AgentPermissions::empty(), tool_permissions, &request);
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
            .dispatch(SubagentDispatchRequest {
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
            })
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
        let dispatch = RuntimeSubagentDispatch::new(store, scheduler).with_registry(registry);

        let result = dispatch
            .dispatch(SubagentDispatchRequest {
                parent_agent_id: AgentId::generate(),
                subagent_type: "instant_timeout".into(),
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
            })
            .await
            .unwrap();

        assert!(matches!(result.exit, SubagentExit::Timeout));
    }
}
