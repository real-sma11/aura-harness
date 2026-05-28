//! Surface-layer [`aura_fleet_spawn::ChildRunner`] impl backed by the
//! engine's [`Scheduler`] + [`Store`] pair.
//!
//! Phase B / Commit 3 extracts the in-process runner from the legacy
//! `aura-runtime/src/subagent_dispatch.rs` god-file. The dispatcher
//! itself moved to `aura-fleet-subagent` (fleet layer); the registry +
//! pure-data adapters moved to `aura-agent-subagent` (agent layer);
//! the surface-layer runner that drives the kernel-mediated agent loop
//! stays at the engine layer.
//!
//! Any future embedded host (e.g. an SDK consumer) can plug a custom
//! [`ChildRunner`] impl into [`aura_fleet_subagent::FleetSubagentDispatcher`]
//! without touching this crate.

use async_trait::async_trait;
use aura_agent::AgentLoopConfig;
use aura_agent_subagent::{narrow_permissions, registry::SubagentRegistry};
use aura_core::{
    resolve_effective_permission, AgentPermissions, AgentToolPermissions, SubagentExit,
    SubagentKindSpec, SubagentResult, ToolState, Transaction, TransactionType, UserDefaultMode,
    UserToolDefaults,
};
use aura_fleet_spawn::{ChildRunContext, ChildRunError, ChildRunner};
use aura_kernel::PolicyConfig;
use aura_store::Store;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use crate::scheduler::Scheduler;

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
    use aura_core::{AgentScope, Capability};

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
}
