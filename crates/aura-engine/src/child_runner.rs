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
use aura_agent_kernel::PolicyConfig;
use aura_agent_loop::AgentLoopConfig;
use aura_agent_subagent::{narrow_permissions, registry::SubagentRegistry};
use aura_core_types::{
    resolve_effective_permission, AgentPermissions, AgentToolPermissions, SubagentExit,
    SubagentKindSpec, SubagentResult, ToolState, Transaction, TransactionType, UserDefaultMode,
    UserToolDefaults,
};
use aura_fleet_spawn::{ChildRunContext, ChildRunError, ChildRunner};
use aura_store_db::Store;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use crate::scheduler::{AgentIdentity, ScheduleOverrides, Scheduler};

/// [`ChildRunner`] implementation backed by the [`Scheduler`] +
/// [`Store`] pair.
pub struct RuntimeChildRunner {
    store: Arc<dyn Store>,
    scheduler: Arc<Scheduler>,
    registry: SubagentRegistry,
    spawn_hook: aura_agent_kernel::KernelSpawnHook,
    /// Optional workspace root override applied to every child run as
    /// `(workspace_base, use_workspace_base_as_root)`. Set to the parent
    /// session's resolved project workspace so subagents share the
    /// parent's sandbox root and can read project files, instead of the
    /// scheduler's empty per-agent scratch directory. `None` preserves
    /// the legacy per-agent `workspace_base/<agent_id>` layout.
    child_workspace: Option<(PathBuf, bool)>,
    /// Optional factory that builds a session-equivalent executor router
    /// for the child run. When set (the gateway path injects it), the
    /// child reuses the real-agent resolver — subagent dispatch, spawn
    /// hooks, caller permissions, and parent-chain — instead of the
    /// scheduler's bare node-level resolver. `None` preserves the legacy
    /// bare-resolver behavior for non-gateway / test callers.
    child_kernel_factory: Option<Arc<dyn crate::child_kernel::ChildKernelFactory>>,
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
    /// scheduler / registry, with no workspace override (children use the
    /// scheduler's default per-agent layout).
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        scheduler: Arc<Scheduler>,
        registry: SubagentRegistry,
    ) -> Self {
        Self {
            spawn_hook: aura_agent_kernel::KernelSpawnHook::new(store.clone()),
            store,
            scheduler,
            registry,
            child_workspace: None,
            child_kernel_factory: None,
        }
    }

    /// Root every child run's sandbox at the given workspace, expressed
    /// as `(workspace_base, use_workspace_base_as_root)` — the same pair
    /// the parent session resolved. This lets subagents read the parent
    /// project's files instead of an empty per-agent scratch directory.
    #[must_use]
    pub fn with_child_workspace(mut self, workspace: PathBuf, use_as_root: bool) -> Self {
        self.child_workspace = Some((workspace, use_as_root));
        self
    }

    /// Inject a [`ChildKernelFactory`](crate::child_kernel::ChildKernelFactory)
    /// so every child run reuses a session-equivalent executor router
    /// (subagent dispatch, spawn hooks, permissions, parent-chain)
    /// instead of the scheduler's bare node resolver.
    #[must_use]
    pub fn with_child_kernel_factory(
        mut self,
        factory: Arc<dyn crate::child_kernel::ChildKernelFactory>,
    ) -> Self {
        self.child_kernel_factory = Some(factory);
        self
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
        let child_spec = aura_agent_kernel::ChildAgentSpec {
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
            use aura_agent_kernel::SpawnHook;
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
                .unwrap_or_else(|| aura_model_reasoner::ENV_FALLBACK_MODEL.to_string())
        } else {
            child_model
        };
        // Clone the parent's full identity (org / session / agent /
        // project / auth) onto the child, overriding only the child's
        // model + system prompt, and keep the merged identity so the
        // child loop config below inherits the `X-Aura-*` envelope.
        let child_identity = self
            .scheduler
            .identity_registry()
            .get(ctx.parent_agent_id)
            .map(|parent| {
                let mut child_identity = parent;
                child_identity.model = child_model.clone();
                child_identity.system_prompt =
                    system_prompt_for(&kind, ctx.spec.system_prompt_addendum.as_deref());
                self.scheduler
                    .identity_registry()
                    .register(child_agent_id, child_identity.clone());
                child_identity
            });
        let loop_config = loop_config_for(&kind, &child_model, child_identity);
        // Build the child's session-equivalent executor router via the
        // injected factory (when present). This is the production seam
        // that gives the child the same subagent dispatch + spawn hooks
        // + caller permissions + parent_chain a real top-level turn
        // gets — replacing the scheduler's bare node resolver for this
        // run. `ctx.parent_chain` is the child's ancestor lineage, so
        // threading it here makes the depth/cycle guards fire on the
        // child's own nested spawns. Built before `policy_for` consumes
        // the narrowed permissions below.
        let router_override = self.child_kernel_factory.as_ref().map(|factory| {
            factory.build_child_router(crate::child_kernel::ChildKernelRequest {
                child_agent_id,
                permissions: child_permissions.clone(),
                tool_permissions: child_tool_permissions.clone(),
                user_tool_defaults: user_defaults.clone(),
                parent_chain: ctx.parent_chain.clone(),
                originating_user_id: originating_user_id.clone(),
                model_id: child_model.clone(),
            })
        });
        let policy = policy_for(child_permissions, child_tool_permissions, &user_defaults);

        // Hand the optional streaming sink to the scheduler so the
        // child loop runs via `run_with_events` and its
        // `AgentLoopEvent`s reach the observer attached to the minted
        // child run id. `None` preserves the non-streaming Wait path.
        let child_event_tx = ctx.event_tx;
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
                self.scheduler.schedule_agent_with_options(
                    child_agent_id,
                    ScheduleOverrides {
                        loop_config: Some(loop_config),
                        policy: Some(policy),
                        event_tx: child_event_tx,
                        workspace_override: self.child_workspace.clone(),
                        router_override,
                    },
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

fn loop_config_for(
    kind: &SubagentKindSpec,
    model: &str,
    identity: Option<AgentIdentity>,
) -> AgentLoopConfig {
    // Prefer the child's merged identity (the parent clone with model +
    // system prompt overridden). `into_loop_config` round-trips the
    // `aura_org_id` / `aura_session_id` / `aura_agent_id` /
    // `aura_project_id` / `auth_token` envelope so the child's outbound
    // `/v1/messages` requests are bucketed per-org/session exactly like
    // the parent turn. The previous bare config left those `None`, so
    // `aura-router` treated the child as anonymous public traffic and
    // returned `429 RATE_LIMITED`. The `None` arm preserves the legacy
    // bare-config behavior for non-gateway / test callers with no
    // registered parent identity.
    let mut config = match identity {
        Some(identity) => identity.into_loop_config(),
        None => AgentLoopConfig {
            system_prompt: kind.system_prompt.clone(),
            ..AgentLoopConfig::for_agent(model)
        },
    };
    config.max_iterations = kind.budget.max_iterations as usize;
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
    use aura_core_types::{AgentScope, Capability};

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

    fn parent_identity(model: &str) -> AgentIdentity {
        AgentIdentity {
            model: model.to_string(),
            aura_org_id: Some("org-parent".to_string()),
            aura_session_id: Some("session-parent".to_string()),
            aura_agent_id: Some("agent-parent".to_string()),
            aura_project_id: Some("project-parent".to_string()),
            system_prompt: "parent prompt".to_string(),
            prompt_cache_key: None,
            prompt_cache_retention: None,
            request_kind: aura_model_reasoner::ModelRequestKind::Chat,
            max_tokens: 1024,
            max_context_tokens: 200_000,
            auth_token: Some("parent-jwt".to_string()),
        }
    }

    #[test]
    fn loop_config_inherits_parent_identity_envelope() {
        let registry = SubagentRegistry::bundled();
        let kind = registry.get("explore").unwrap();
        let identity = parent_identity("claude-opus-4-7");

        let config = loop_config_for(kind, "claude-opus-4-7", Some(identity));

        // The child's outbound `/v1/messages` requests must carry the
        // parent's `X-Aura-*` / auth envelope so `aura-router` buckets
        // them per-org/session instead of returning a public 429.
        assert_eq!(config.aura_org_id.as_deref(), Some("org-parent"));
        assert_eq!(config.aura_session_id.as_deref(), Some("session-parent"));
        assert_eq!(config.aura_agent_id.as_deref(), Some("agent-parent"));
        assert_eq!(config.aura_project_id.as_deref(), Some("project-parent"));
        assert_eq!(config.auth_token.as_deref(), Some("parent-jwt"));
        // Subagent budget still wins over the inherited identity.
        assert_eq!(config.max_iterations, kind.budget.max_iterations as usize);
        if let Some(max_tokens) = kind.budget.max_tokens {
            assert_eq!(config.max_tokens, max_tokens);
        }
    }

    #[test]
    fn loop_config_without_identity_falls_back_to_bare_config() {
        let registry = SubagentRegistry::bundled();
        let kind = registry.get("explore").unwrap();

        let config = loop_config_for(kind, "claude-opus-4-7", None);

        assert!(config.aura_org_id.is_none());
        assert!(config.aura_session_id.is_none());
        assert!(config.auth_token.is_none());
        assert_eq!(config.max_iterations, kind.budget.max_iterations as usize);
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
