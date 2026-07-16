//! Tool executor implementation.

use crate::error::ToolError;
use crate::sandbox::Sandbox;
use crate::tool::{
    builtin_tools, AgentControlHook, AgentReadHook, SubagentDispatchHook, Tool, ToolContext,
};
use crate::ToolConfig;
use async_trait::async_trait;
use aura_core_types::{
    Action, ActionKind, AgentId, AgentPermissions, AgentToolPermissions, Effect, EffectKind,
    EffectStatus, ToolCall, ToolResult, UserToolDefaults,
};
use aura_exec_traits::{ExecuteContext, Executor, ExecutorError, SpawnHook};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, error, instrument};

/// Tool executor for filesystem and command operations.
///
/// Holds a `HashMap<String, Box<dyn Tool>>` for trait-based dispatch
/// instead of a hardcoded match block.
///
/// Phase 5: optional `spawn_hook` / `agent_control_hook` / `agent_read_hook`
/// are injected into every [`ToolContext`] this executor creates so
/// cross-agent tools can actually perform their runtime side-effects. Each
/// hook defaults to `None`, preserving pre-phase-5 behavior.
pub struct ToolExecutor {
    config: ToolConfig,
    tools: HashMap<String, Box<dyn Tool>>,
    spawn_hook: Option<Arc<dyn SpawnHook>>,
    agent_control_hook: Option<Arc<dyn AgentControlHook>>,
    agent_read_hook: Option<Arc<dyn AgentReadHook>>,
    subagent_dispatch: Option<Arc<dyn SubagentDispatchHook>>,
    caller_permissions: Option<AgentPermissions>,
    caller_tool_permissions: Option<AgentToolPermissions>,
    user_tool_defaults: UserToolDefaults,
    parent_chain: Vec<AgentId>,
    originating_user_id: Option<String>,
    /// Caller's upstream OS UUID (e.g. `aura-os-server`'s `agents.agent_id`).
    /// Forwarded into [`ToolContext::caller_external_agent_id`] so the
    /// cross-agent tools can ship a server-resolvable id as
    /// `originating_agent_id` / `parent_agent_id` instead of the truncated
    /// harness hash. See the field doc on `ToolContext` for the full
    /// rationale and the matching server-side wiring.
    caller_external_agent_id: Option<String>,
    /// Active aura-os project id for project-scoped cross-agent delivery.
    caller_project_id: Option<String>,
    /// Caller's resolved model id, forwarded into
    /// [`ToolContext::caller_model_id`] so cross-agent tools can ship
    /// the caller's model to the target agent's turn (the recipient
    /// usually has no server-side configured model). `None` leaves the
    /// downstream null-model behavior unchanged.
    caller_model_id: Option<String>,
}

impl ToolExecutor {
    /// Create a new tool executor with the given config and all builtin tools.
    #[must_use]
    pub fn new(config: ToolConfig) -> Self {
        let mut tools = HashMap::new();
        for tool in builtin_tools() {
            tools.insert(tool.name().to_string(), tool);
        }
        for (tool, _definition, _required) in crate::agents::cross_agent_catalog_entries() {
            tools.insert(tool.name().to_string(), tool);
        }
        Self {
            config,
            tools,
            spawn_hook: None,
            agent_control_hook: None,
            agent_read_hook: None,
            subagent_dispatch: None,
            caller_permissions: None,
            caller_tool_permissions: None,
            user_tool_defaults: UserToolDefaults::default(),
            parent_chain: Vec::new(),
            originating_user_id: None,
            caller_external_agent_id: None,
            caller_project_id: None,
            caller_model_id: None,
        }
    }

    /// Phase 5: attach a [`SpawnHook`] that the `spawn_agent` tool will use
    /// to persist new child agents.
    #[must_use]
    pub fn with_spawn_hook(mut self, hook: Arc<dyn SpawnHook>) -> Self {
        self.spawn_hook = Some(hook);
        self
    }

    /// Phase 5: attach an [`AgentControlHook`] for `send_to_agent` /
    /// `agent_lifecycle` / `delegate_task`.
    #[must_use]
    pub fn with_agent_control_hook(mut self, hook: Arc<dyn AgentControlHook>) -> Self {
        self.agent_control_hook = Some(hook);
        self
    }

    /// Phase 5: attach an [`AgentReadHook`] for `get_agent_state`.
    #[must_use]
    pub fn with_agent_read_hook(mut self, hook: Arc<dyn AgentReadHook>) -> Self {
        self.agent_read_hook = Some(hook);
        self
    }

    /// Attach a foreground subagent dispatcher for the `task` tool.
    #[must_use]
    pub fn with_subagent_dispatch_hook(mut self, hook: Arc<dyn SubagentDispatchHook>) -> Self {
        self.subagent_dispatch = Some(hook);
        self
    }

    /// Phase 5: set the caller's permissions (scope + capabilities). Used
    /// by cross-agent tools to enforce strict-subset and scope checks.
    #[must_use]
    pub fn with_caller_permissions(mut self, permissions: AgentPermissions) -> Self {
        self.caller_permissions = Some(permissions);
        self
    }

    /// Attach the caller's current per-tool override context for
    /// monotonic child-agent spawn checks.
    #[must_use]
    pub fn with_tool_permission_context(
        mut self,
        user_default: UserToolDefaults,
        permissions: Option<AgentToolPermissions>,
    ) -> Self {
        self.user_tool_defaults = user_default;
        self.caller_tool_permissions = permissions;
        self
    }

    /// Phase 5: set the caller's ancestor chain for cycle prevention in
    /// `spawn_agent`.
    #[must_use]
    pub fn with_parent_chain(mut self, chain: Vec<AgentId>) -> Self {
        self.parent_chain = chain;
        self
    }

    /// Phase 5: set the originating end-user id that started this delegate
    /// chain. Propagated onto every `Delegate`-tagged transaction.
    #[must_use]
    pub fn with_originating_user_id(mut self, user: impl Into<String>) -> Self {
        self.originating_user_id = Some(user.into());
        self
    }

    /// Set the caller's **external** agent id — the upstream OS UUID
    /// (`aura-os-server`'s `agents.agent_id`) that identifies this agent
    /// on the OS REST surface. The harness's internal
    /// [`aura_core_types::AgentId`] is a 32-byte blake3 hash of that UUID
    /// (see `aura_core_types::AgentId::from_uuid` and the harness runtime-request
    /// fallback in `crates/aura-runtime/src/gateway/session/state.rs`), and its
    /// `Display` impl truncates to 16 hex chars — so passing
    /// `caller_agent_id.to_string()` to `aura-os-server` as
    /// `originating_agent_id` is unparseable as a UUID at the
    /// `Path<AgentId>` extractor and silently fails the cross-agent
    /// async-reply callback. Wire this with the un-hashed UUID
    /// (typically `SessionState::skill_agent_id` populated from
    /// `RuntimeRequest.agent_identity.template_id`) so `send_to_agent` ships a value the
    /// server can route.
    #[must_use]
    pub fn with_caller_external_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        let value = agent_id.into();
        if !value.trim().is_empty() {
            self.caller_external_agent_id = Some(value);
        }
        self
    }

    /// Set the active aura-os project for cross-agent delivery. Blank values
    /// are ignored so direct, project-less chats retain their legacy route.
    #[must_use]
    pub fn with_caller_project_id(mut self, project_id: impl Into<String>) -> Self {
        let value = project_id.into();
        if !value.trim().is_empty() {
            self.caller_project_id = Some(value);
        }
        self
    }

    /// Set the caller's resolved model id. Forwarded into
    /// [`ToolContext::caller_model_id`] so cross-agent tools
    /// (`send_to_agent`, `delegate_task`) can ship the caller's model
    /// to the target agent's turn. Blank/whitespace is treated as
    /// unset so the downstream null-model wire value is preserved.
    #[must_use]
    pub fn with_caller_model_id(mut self, model_id: impl Into<String>) -> Self {
        let value = model_id.into();
        if !value.trim().is_empty() {
            self.caller_model_id = Some(value);
        }
        self
    }

    /// Create a tool executor with default config.
    ///
    /// **Phase 5 hardening note:** [`ToolConfig::default`] now yields a
    /// fail-closed command configuration — command execution is disabled,
    /// `allow_shell` is `false`, and every allow-list (`binary_allowlist`,
    /// `command_allowlist`, `allowed_shell_scripts`) is empty. Any call to
    /// `run_command` through an executor built with `with_defaults()` will
    /// therefore be refused by [`CmdRunTool::execute`].
    ///
    /// Callers that genuinely need command execution must construct a
    /// custom [`ToolConfig`] with `command.enabled: true` **and** a
    /// populated `binary_allowlist`. There is no helper for this on
    /// purpose — opt-in must be deliberate.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(ToolConfig::default())
    }

    /// Borrow the current tool configuration.
    #[must_use]
    pub fn config(&self) -> &ToolConfig {
        &self.config
    }

    /// Check whether a tool handler is registered by name.
    #[must_use]
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Register an additional tool at runtime.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Execute a tool call with permission checks and sandbox enforcement.
    //
    // `tool_call` is in the skip list to keep credential-bearing `args`
    // (`jwt`, bearer tokens, etc.) out of span fields. The tool name
    // is surfaced separately via the `fields(tool = ...)` attribute.
    // `ToolCall` also has a redacting `Debug` impl as defense in depth.
    #[instrument(skip(self, ctx, tool_call), fields(tool = %tool_call.tool))]
    pub async fn execute_tool(
        &self,
        ctx: &ExecuteContext,
        tool_call: &ToolCall,
    ) -> Result<ToolResult, ToolError> {
        let tool_name = &tool_call.tool;

        let workspace_root = ctx.workspace_root.clone();
        let extra_paths = self.config.extra_allowed_paths.clone();
        // Privileged dev environment: an effectively-full-access session
        // (operator opt-in via `AURA_ALLOW_UNRESTRICTED_FULL_ACCESS` plus
        // a FullAccess agent, surfaced here as `bypass_allowlists`) is
        // granted an OS-wide filesystem root so the agent can modify the
        // guest OS — install packages, edit `/etc`, write `/usr`, etc.
        // This widens the same containment that otherwise pins every tool
        // to the workspace root. It is gated to the existing full-access
        // opt-in and is only safe behind microVM isolation (the K8s
        // securityContext decides whether this process is root); ordinary
        // agents keep strict per-tenant workspace containment.
        let os_wide = self.config.command.bypass_allowlists;
        let sandbox = tokio::task::spawn_blocking(move || {
            if os_wide {
                // `/` targets the Linux microVM guest rootfs. Extra
                // skill-granted paths are still merged for parity.
                let mut roots = Vec::with_capacity(extra_paths.len() + 1);
                roots.push(std::path::PathBuf::from("/"));
                roots.extend(extra_paths);
                Sandbox::with_extra_roots(&workspace_root, &roots)
            } else if extra_paths.is_empty() {
                Sandbox::new(&workspace_root)
            } else {
                Sandbox::with_extra_roots(&workspace_root, &extra_paths)
            }
        })
        .await
        .map_err(|e| ToolError::CommandFailed(format!("sandbox init task panicked: {e}")))??;
        let mut tool_ctx = ToolContext::new(sandbox, self.config.clone());
        tool_ctx.caller_agent_id = Some(ctx.agent_id);
        tool_ctx.caller_external_agent_id = self.caller_external_agent_id.clone();
        tool_ctx.caller_project_id = self.caller_project_id.clone();
        tool_ctx.caller_model_id = self.caller_model_id.clone();
        tool_ctx.caller_permissions = self.caller_permissions.clone();
        tool_ctx.caller_tool_permissions = self.caller_tool_permissions.clone();
        tool_ctx.user_tool_defaults = self.user_tool_defaults.clone();
        tool_ctx.parent_chain = self.parent_chain.clone();
        tool_ctx.originating_user_id = self.originating_user_id.clone();
        tool_ctx.spawn_hook = self.spawn_hook.clone();
        tool_ctx.agent_control_hook = self.agent_control_hook.clone();
        tool_ctx.agent_read_hook = self.agent_read_hook.clone();
        tool_ctx.subagent_dispatch = self.subagent_dispatch.clone();
        tool_ctx.current_tool_use_id = ctx.tool_use_id.clone();

        match self.tools.get(tool_name.as_str()) {
            Some(tool) => tool.execute(&tool_ctx, tool_call.args.clone()).await,
            None => Err(ToolError::UnknownTool(tool_name.clone())),
        }
    }
}

#[async_trait]
impl Executor for ToolExecutor {
    #[instrument(skip(self, ctx, action), fields(action_id = %action.action_id))]
    async fn execute(
        &self,
        ctx: &ExecuteContext,
        action: &Action,
    ) -> Result<Effect, ExecutorError> {
        let tool_call: ToolCall = serde_json::from_slice(&action.payload).map_err(|e| {
            ExecutorError::ExecutionFailed(format!("Failed to parse tool call: {e}"))
        })?;

        debug!(tool = %tool_call.tool, "Executing tool");

        match self.execute_tool(ctx, &tool_call).await {
            Ok(result) => {
                let payload = serde_json::to_vec(&result).map_err(|e| {
                    ExecutorError::ExecutionFailed(format!("Failed to serialize tool result: {e}"))
                })?;
                Ok(Effect::new(
                    action.action_id,
                    EffectKind::Agreement,
                    EffectStatus::Committed,
                    Bytes::from(payload),
                ))
            }
            Err(e) => {
                error!(error = %e, "Tool execution failed");
                let result = match e {
                    ToolError::CompactionStructural(msg) => {
                        ToolResult::compaction_structural_failure(&tool_call.tool, msg)
                    }
                    other => ToolResult::failure(&tool_call.tool, other.to_string()),
                };
                let payload = serde_json::to_vec(&result).map_err(|e| {
                    ExecutorError::ExecutionFailed(format!("Failed to serialize error result: {e}"))
                })?;
                Ok(Effect::new(
                    action.action_id,
                    EffectKind::Agreement,
                    EffectStatus::Failed,
                    Bytes::from(payload),
                ))
            }
        }
    }

    fn can_handle(&self, action: &Action) -> bool {
        if action.kind != ActionKind::Delegate {
            return false;
        }
        serde_json::from_slice::<ToolCall>(&action.payload).is_ok()
    }

    fn name(&self) -> &'static str {
        "tool"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core_types::{ActionId, AgentId};
    use aura_exec_traits::ExecuteContext;
    use tempfile::TempDir;

    fn create_test_context() -> (ExecuteContext, TempDir) {
        let dir = TempDir::new().unwrap();
        let ctx = ExecuteContext::new(
            AgentId::generate(),
            ActionId::generate(),
            dir.path().to_path_buf(),
        );
        (ctx, dir)
    }

    #[tokio::test]
    async fn test_fs_ls_tool() {
        let (ctx, dir) = create_test_context();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let executor = ToolExecutor::with_defaults();
        let tool_call = ToolCall::fs_ls(".");
        let action = Action::delegate_tool(&tool_call).unwrap();

        let effect = executor.execute(&ctx, &action).await.unwrap();
        assert_eq!(effect.status, EffectStatus::Committed);

        let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
        assert!(result.ok);
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("test.txt"));
    }

    #[tokio::test]
    async fn test_fs_read_tool() {
        let (ctx, dir) = create_test_context();
        std::fs::write(dir.path().join("test.txt"), "Hello, Aura!").unwrap();

        let executor = ToolExecutor::with_defaults();
        let tool_call = ToolCall::fs_read("test.txt", None);
        let action = Action::delegate_tool(&tool_call).unwrap();

        let effect = executor.execute(&ctx, &action).await.unwrap();
        assert_eq!(effect.status, EffectStatus::Committed);

        let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
        assert!(result.ok);
        assert_eq!(&result.stdout[..], b"Hello, Aura!");
    }

    #[tokio::test]
    async fn test_sandbox_violation() {
        let (ctx, _dir) = create_test_context();

        let executor = ToolExecutor::with_defaults();
        let tool_call = ToolCall::fs_read("../../../etc/passwd", None);
        let action = Action::delegate_tool(&tool_call).unwrap();

        let effect = executor.execute(&ctx, &action).await.unwrap();
        assert_eq!(effect.status, EffectStatus::Failed);

        let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_cmd_disabled() {
        let (ctx, _dir) = create_test_context();

        let config = ToolConfig::default();
        let executor = ToolExecutor::new(config);
        let tool_call = ToolCall::new("run_command", serde_json::json!({"program": "ls"}));
        let action = Action::delegate_tool(&tool_call).unwrap();

        let effect = executor.execute(&ctx, &action).await.unwrap();
        assert_eq!(effect.status, EffectStatus::Failed);
    }

    #[tokio::test]
    async fn test_unknown_tool() {
        let (ctx, _dir) = create_test_context();

        let executor = ToolExecutor::with_defaults();
        let tool_call = ToolCall::new("nonexistent_tool", serde_json::json!({}));
        let action = Action::delegate_tool(&tool_call).unwrap();

        let effect = executor.execute(&ctx, &action).await.unwrap();
        assert_eq!(effect.status, EffectStatus::Failed);

        let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn test_register_custom_tool() {
        let mut executor = ToolExecutor::with_defaults();
        assert!(executor.tools.contains_key("list_files"));
        assert!(!executor.tools.contains_key("custom_tool"));

        // Custom tools can be registered at runtime
        struct DummyTool;

        #[async_trait]
        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "custom_tool"
            }
            fn definition(&self) -> aura_core_types::ToolDefinition {
                aura_core_types::ToolDefinition::new(
                    "custom_tool",
                    "A test tool",
                    serde_json::json!({"type": "object"}),
                )
            }
            async fn execute(
                &self,
                _ctx: &ToolContext,
                _args: serde_json::Value,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::success("custom_tool", "ok"))
            }
        }

        executor.register(Box::new(DummyTool));
        assert!(executor.tools.contains_key("custom_tool"));
    }
}
