//! Extensible tool trait for trait-based dispatch.
//!
//! Each tool is a struct implementing [`Tool`], providing its name,
//! JSON schema definition, and execution logic. The [`ToolExecutor`](crate::ToolExecutor)
//! dispatches to tools via `HashMap` lookup instead of a hardcoded match.

use crate::error::ToolError;
use crate::sandbox::Sandbox;
use crate::ToolConfig;
use async_trait::async_trait;
use aura_core_types::{
    AgentId, AgentMode, AgentPermissions, AgentToolPermissions, Capability, KernelMode,
    SubagentDispatchRequest, SubagentResult, ToolDefinition, ToolResult, UserToolDefaults,
};
use aura_exec_traits::SpawnHook;
use std::sync::Arc;

/// Context provided to tools during execution.
pub struct ToolContext {
    /// Sandbox for path validation and resolution.
    pub sandbox: Sandbox,
    /// Tool configuration (limits, permissions).
    pub config: ToolConfig,
    /// Phase 5: agent id of the caller that issued this tool call, when
    /// known. Cross-agent tools read this to populate parent-chain metadata
    /// on the resulting transaction.
    pub caller_agent_id: Option<AgentId>,
    /// Caller's **external** agent id — the upstream OS UUID that the
    /// outer system (aura-os-server) uses to identify this agent on its
    /// REST surface. Distinct from [`Self::caller_agent_id`], which is the
    /// harness's internal `aura_core_types::AgentId` (a 32-byte blake3 hash whose
    /// `Display` impl truncates to 16 hex chars and is irreversible to the
    /// upstream UUID).
    ///
    /// Cross-agent tools (`send_to_agent`, `delegate_task`,
    /// `agent_lifecycle`) ship this value to aura-os-server as
    /// `originating_agent_id` / `parent_agent_id` so the server-side
    /// async-reply callback (`spawn_cross_agent_reply_callback` in
    /// `apps/aura-os-server/src/handlers/agents/chat/cross_agent_reply.rs`)
    /// can POST the recipient's reply back into the originator's session at
    /// `/api/agents/{originating_agent_id}/events/stream`. The route is
    /// declared `Path<AgentId>` where `AgentId` is a `Uuid`, so passing the
    /// truncated harness hash here is a silent 400 — the reply is lost.
    /// Populated by the runtime from `SessionState::skill_agent_id`
    /// (which carries `RuntimeRequest.agent_identity.template_id` when set,
    /// otherwise the raw runtime agent id).
    pub caller_external_agent_id: Option<String>,
    /// Project that owns the caller's active conversation. Cross-agent
    /// delivery forwards this value to aura-os-server so an agent reused
    /// across projects receives the message in the originating project.
    pub caller_project_id: Option<String>,
    /// Phase 5: caller's scope + capability grants. Cross-agent tools (e.g.
    /// `spawn_agent`) enforce strict-subset semantics against this bundle.
    pub caller_permissions: Option<AgentPermissions>,
    /// Caller per-agent tool override, when present for this session.
    pub caller_tool_permissions: Option<AgentToolPermissions>,
    /// User default used with `caller_tool_permissions` for monotonic spawn
    /// checks.
    pub user_tool_defaults: UserToolDefaults,
    /// Phase 5: ancestor chain for the caller (immediate parent first, root
    /// last). Used for cycle prevention in `spawn_agent`.
    pub parent_chain: Vec<AgentId>,
    /// Phase 5: originating end-user id that began this delegate chain.
    /// Propagated onto every Delegate transaction for billing attribution.
    pub originating_user_id: Option<String>,
    /// Phase 5 part 2: optional spawn hook used by the `spawn_agent` tool to
    /// actually persist a new child agent. `None` means "no hook wired" â€” the
    /// tool returns a pure outcome payload without touching a store.
    pub spawn_hook: Option<Arc<dyn SpawnHook>>,
    /// Phase 5 part 2: optional cross-agent control hook used by
    /// `send_to_agent`, `agent_lifecycle`, and `delegate_task` to deliver
    /// effects to the target agent. `None` means the tool fails closed after
    /// the permission gate instead of reporting a fake no-op success.
    pub agent_control_hook: Option<Arc<dyn AgentControlHook>>,
    /// Phase 5 part 2: optional read hook used by `get_agent_state` and
    /// `list_agents` to fetch read-only agent data.
    pub agent_read_hook: Option<Arc<dyn AgentReadHook>>,
    /// Optional runtime dispatch hook for foreground `task` subagents.
    /// `None` means the `task` tool fails closed.
    pub subagent_dispatch: Option<Arc<dyn SubagentDispatchHook>>,
    /// Phase 7b: caller's resolved [`AgentMode`] snapshot. Threaded
    /// to the `task` tool so child derivation can enforce mode
    /// narrowing.
    pub caller_mode: Option<AgentMode>,
    /// Phase 7b: caller's resolved [`KernelMode`] snapshot.
    pub caller_kernel_mode: Option<KernelMode>,
    /// Phase 7b: caller's resolved model identifier snapshot.
    pub caller_model_id: Option<String>,
    /// Model-supplied `tool_use` id of the tool call currently being
    /// executed, when known. Threaded from
    /// [`ExecuteContext::tool_use_id`](aura_exec_traits::ExecuteContext)
    /// so the `task` tool can use the real tool-use block id as the
    /// subagent dispatch `tool_call_id` — this is what the UI binds the
    /// spawned subagent card to (`parent_tool_use_id`). `None` falls
    /// back to any caller-stamped dedupe key on the tool input.
    pub current_tool_use_id: Option<String>,
}

/// Hook invoked by the `send_to_agent` / `agent_lifecycle` / `delegate_task`
/// tools to actually affect the target agent. Kept as a trait so the
/// permission gate can be tested without wiring a real kernel writer.
#[async_trait]
pub trait AgentControlHook: Send + Sync {
    /// Deliver a user-message-shaped payload to `target`.
    ///
    /// `model` is the **caller's** resolved model id (from
    /// [`ToolContext::caller_model_id`]). It is forwarded so the
    /// target's turn runs on a real model: cross-agent recipients
    /// often have no server-side configured model (the UI sends the
    /// model per-turn from client state), so omitting it leaves the
    /// recipient's harness session with an empty model and the turn
    /// fails with "model name must not be empty". `None` preserves the
    /// legacy null-model wire value for callers without a known model.
    async fn deliver_message(
        &self,
        target_agent_id: &str,
        parent_agent_id: Option<&str>,
        originating_user_id: Option<&str>,
        project_id: Option<&str>,
        content: &str,
        attachments: Option<serde_json::Value>,
        model: Option<&str>,
    ) -> Result<(), String>;

    /// Apply a lifecycle transition to `target`.
    async fn lifecycle(
        &self,
        target_agent_id: &str,
        parent_agent_id: Option<&str>,
        originating_user_id: Option<&str>,
        action: &str,
    ) -> Result<(), String>;

    /// Emit a Delegate-tagged task to `target`.
    ///
    /// `model` mirrors [`Self::deliver_message`]: the caller's resolved
    /// model id so the delegated turn runs on a real model.
    async fn delegate_task(
        &self,
        target_agent_id: &str,
        parent_agent_id: Option<&str>,
        originating_user_id: Option<&str>,
        task: &str,
        context: Option<&serde_json::Value>,
        model: Option<&str>,
    ) -> Result<(), String>;
}

/// Hook used by read-only cross-agent tools. Kept as a trait so the gate is
/// testable without a kernel.
#[async_trait]
pub trait AgentReadHook: Send + Sync {
    /// Return the latest `session_ready` / `assistant_message_end`
    /// snapshot for `target`, plus the agent's `Identity` + `permissions`.
    async fn snapshot(&self, target_agent_id: &str) -> Result<serde_json::Value, String>;

    /// Return agents visible to the current caller, optionally narrowed by org.
    async fn list_agents(&self, org_id: Option<&str>) -> Result<serde_json::Value, String>;
}

/// Hook invoked by the `task` tool to run a foreground subagent.
///
/// The trait intentionally uses only `aura-core` data types so `aura-tools`
/// remains independent from `aura-runtime`, `aura-agent` event types, and
/// transport protocols.
#[async_trait]
pub trait SubagentDispatchHook: Send + Sync {
    async fn dispatch(&self, request: SubagentDispatchRequest) -> Result<SubagentResult, String>;
}

impl ToolContext {
    /// Construct a minimal context with only the fields required pre-phase-5.
    /// All new cross-agent fields default to `None` / empty.
    #[must_use]
    pub fn new(sandbox: Sandbox, config: ToolConfig) -> Self {
        Self {
            sandbox,
            config,
            caller_agent_id: None,
            caller_external_agent_id: None,
            caller_project_id: None,
            caller_permissions: None,
            caller_tool_permissions: None,
            user_tool_defaults: UserToolDefaults::default(),
            parent_chain: Vec::new(),
            originating_user_id: None,
            spawn_hook: None,
            agent_control_hook: None,
            agent_read_hook: None,
            subagent_dispatch: None,
            caller_mode: None,
            caller_kernel_mode: None,
            caller_model_id: None,
            current_tool_use_id: None,
        }
    }
}

/// Trait for extensible tool implementations.
///
/// The `ToolExecutor` holds a `HashMap<String, Box<dyn Tool>>` and dispatches
/// calls by name lookup. Built-in tools and external tools both implement
/// this trait.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name used for dispatch (e.g., "read_file", "run_command").
    fn name(&self) -> &str;

    /// JSON schema definition sent to the model.
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with parsed arguments.
    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError>;

    /// Phase 5: capabilities required on the caller's `AgentPermissions` for
    /// this tool to be visible + callable. Default is empty (tool is
    /// universally visible, matching pre-phase-5 behavior).
    ///
    /// `ToolCatalog::visible_tools` filters the catalog against this set;
    /// the kernel `Policy` layer additionally enforces it at proposal time
    /// via `PolicyConfig::tool_capability_requirements`.
    fn required_capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
}

/// Returns all built-in tool instances.
pub fn builtin_tools() -> Vec<Box<dyn Tool>> {
    use crate::fs_tools::{
        CmdRunTool, FsDeleteTool, FsEditTool, FsFindTool, FsLsTool, FsReadTool, FsStatTool,
        FsWriteTool, SearchCodeTool,
    };

    vec![
        Box::new(FsLsTool),
        Box::new(FsReadTool),
        Box::new(FsStatTool),
        Box::new(FsWriteTool),
        Box::new(FsEditTool),
        Box::new(FsDeleteTool),
        Box::new(FsFindTool),
        Box::new(SearchCodeTool),
        Box::new(CmdRunTool),
        Box::new(crate::git_tool::GitCommitTool),
        Box::new(crate::git_tool::GitPushTool),
        Box::new(crate::git_tool::GitCommitPushTool),
    ]
}
