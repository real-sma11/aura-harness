//! Helper functions for WebSocket session management: init, executor
//! construction, event forwarding, and turn finalization.

use super::chat::populate_tool_definitions;
use super::{Session, WsContext};
use crate::gateway::session::cross_agent_hook::{AuraServerAgentHook, AuraServerSpawnHook};
use crate::protocol::{
    tool_info_from_definition_with_state, AssistantMessageEnd, ContextBreakdown, ContextContents,
    ContextSegment, ErrorMsg, FileDiff, FilesChanged, OutboundMessage, SessionReady, SessionUsage,
    SkillInfo, TextDelta, ThinkingDelta, ToolCallSnapshot, ToolInfo, ToolResultMsg, ToolUseStart,
};
use async_trait::async_trait;
use aura_agent::{
    map_agent_loop_event, AgentLoopEvent, AgentLoopResult, DebugEvent, TurnEventSink,
};
use aura_agent_kernel::{Kernel, KernelConfig, PolicyConfig};
use aura_agent_subagent::SubagentRegistry;
use aura_core_types::{
    is_effectively_full_access, resolve_effective_permission, AgentToolPermissions, ToolState,
    UserToolDefaults,
};
use aura_engine::{capabilities, child_runner::RuntimeChildRunner, executor};
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{OrphanStore, ParentLeaseRegistry};
use aura_fleet_subagent::FleetSubagentDispatcher;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const OUTBOUND_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_DELTA_COALESCE_BYTES: usize = 512;

/// Convert agent-side context segments into their wire equivalents.
fn convert_context_segments(
    segments: &[aura_agent::types::AgentContextSegment],
) -> Vec<ContextSegment> {
    segments
        .iter()
        .map(|seg| ContextSegment {
            label: seg.label.clone(),
            text: seg.text.clone(),
            tokens: seg.tokens,
        })
        .collect()
}

/// Convert the loop's per-turn rendered context contents into the wire
/// [`ContextContents`]. Returns `None` when every bucket is empty so
/// the emitted `SessionUsage` omits the field (matching older-style
/// empty turns) rather than carrying an all-empty payload.
fn build_wire_context_contents(loop_result: &AgentLoopResult) -> Option<ContextContents> {
    let src = &loop_result.context_contents;
    let contents = ContextContents {
        system_prompt: src.system_prompt.clone(),
        tools: convert_context_segments(&src.tools),
        skills: convert_context_segments(&src.skills),
        subagents: convert_context_segments(&src.subagents),
        mcp: convert_context_segments(&src.mcp),
    };
    if contents.is_empty() {
        None
    } else {
        Some(contents)
    }
}

fn summarize_files_changed(loop_result: &AgentLoopResult) -> FilesChanged {
    let mut files_changed = FilesChanged::default();
    for change in &loop_result.file_changes {
        match change.kind {
            aura_agent::FileChangeKind::Create => files_changed.created.push(change.path.clone()),
            aura_agent::FileChangeKind::Modify => files_changed.modified.push(change.path.clone()),
            aura_agent::FileChangeKind::Delete => files_changed.deleted.push(change.path.clone()),
        }
        // Emit a per-path diff entry whenever the harness produced
        // non-zero counts. Tools that don't compute a diff (write_file
        // / delete_file today) leave both at 0 and we skip them — the
        // dashboard treats absence of a diff entry as "unknown" rather
        // than "zero lines changed", so omitting beats inserting a
        // misleading 0/0.
        if change.lines_added > 0 || change.lines_removed > 0 {
            files_changed.diffs.push(FileDiff {
                path: change.path.clone(),
                lines_added: change.lines_added,
                lines_removed: change.lines_removed,
            });
        }
    }
    files_changed
}

fn resolve_session_workspace(session: &Session) -> (std::path::PathBuf, bool) {
    if let Some(ref project_path) = session.project_path {
        return (project_path.clone(), true);
    }

    if session.workspace != session.workspace_base {
        return (session.workspace.clone(), true);
    }

    (session.workspace.clone(), false)
}

pub(super) fn session_user_defaults(
    session: &Session,
    ctx: &WsContext,
) -> Result<UserToolDefaults, aura_agent_kernel::KernelError> {
    ctx.store
        .get_user_tool_defaults(&session.user_id)
        .map_err(|e| aura_agent_kernel::KernelError::Store(format!("get_user_tool_defaults: {e}")))
        .map(|defaults| defaults.unwrap_or_default())
}

fn session_scoped_tool_config(
    base: &aura_tools::ToolConfig,
    user_default: &UserToolDefaults,
    agent_override: Option<&AgentToolPermissions>,
) -> aura_tools::ToolConfig {
    let mut config = base.clone();
    config.command.bypass_allowlists = base.command.allow_unrestricted_full_access
        && is_effectively_full_access(user_default, agent_override);
    config
}

fn effective_tool_infos(session: &Session, defaults: &UserToolDefaults) -> Vec<ToolInfo> {
    session
        .tool_definitions
        .iter()
        .filter_map(|tool| {
            let state = resolve_effective_permission(
                defaults,
                session.tool_permissions.as_ref(),
                &tool.name,
            );
            (state != ToolState::Deny).then(|| tool_info_from_definition_with_state(tool, state))
        })
        .collect()
}

/// Error returned when building a chat [`Session`] from a
/// [`RuntimeRequest`] fails before the WS attaches.
///
/// Carries a `code` matching the legacy `OutboundMessage::Error.code`
/// strings so the HTTP error path emits the same diagnostic surface
/// the pre-`POST /v1/run` flow used.
#[derive(Debug)]
pub(crate) struct ChatRequestError {
    pub code: &'static str,
    pub message: String,
}

/// Build a fully-applied chat [`Session`] from a [`RuntimeRequest`]
/// before the WebSocket attaches.
///
/// Replaces the pre-Phase-A `handle_session_init` flow. Validation
/// errors (invalid workspace, malformed provider overrides, etc.)
/// are returned as a [`ChatRequestError`] so the gateway can surface
/// them as a 4xx HTTP response.
pub(crate) async fn prepare_chat_session(
    request: aura_protocol::RuntimeRequest,
    ctx: &WsContext,
) -> Result<Session, ChatRequestError> {
    let provider_overrides = request.model.provider_overrides.clone();

    let resolved_provider_override = if let Some(overrides) = provider_overrides {
        let reasoner_overrides = aura_model_reasoner::SessionOverrides {
            default_model: overrides.default_model.clone(),
            fallback_model: overrides.fallback_model.clone(),
            prompt_caching_enabled: overrides.prompt_caching_enabled,
            prompt_cache_key: overrides.prompt_cache_key.clone(),
            prompt_cache_retention: overrides.prompt_cache_retention.clone(),
        };
        match aura_model_reasoner::with_session_overrides(reasoner_overrides) {
            Ok(selection) => Some(selection.provider),
            Err(e) => {
                return Err(ChatRequestError {
                    code: "invalid_provider_config",
                    message: e.to_string(),
                });
            }
        }
    } else {
        None
    };

    let mut session = Session::new(ctx.workspace_base.clone());
    session.project_base = ctx.project_base.clone();
    session.auth_token = ctx.auth_token.clone();

    if let Err(e) = session.apply_chat_runtime_request(request) {
        return Err(ChatRequestError {
            code: "invalid_workspace",
            message: e,
        });
    }

    if session.tool_permissions.is_none() {
        let tool_context =
            crate::tool_permissions::load_agent_tool_context(ctx.store.as_ref(), session.agent_id);
        match tool_context {
            Ok(agent_ctx) => {
                session.tool_permissions = agent_ctx.tool_permissions;
            }
            Err(e) => {
                return Err(ChatRequestError {
                    code: "tool_permissions_load_failed",
                    message: e,
                });
            }
        }
    }

    if let Some(provider) = resolved_provider_override {
        session.provider_name = provider.name().to_string();
        session.provider_override = Some(provider);
    }

    if let (Some(ref base), Some(ref pp)) = (&ctx.project_base, &session.project_path) {
        let slug = pp.file_name().and_then(|n| n.to_str()).unwrap_or("default");
        session.project_path = Some(base.join(slug));
    }

    populate_tool_definitions(&mut session, ctx);

    Ok(session)
}

/// Bootstrap an already-applied chat [`Session`] now that its WS is
/// attached.
///
/// Emits the per-session kernel bootstrap (the `SessionStart`
/// transaction + runtime-capability install record), publishes the
/// identity to the runtime scheduler registry, and sends the
/// terminal `SessionReady` frame so the client can start streaming
/// `user_message` traffic.
pub(super) async fn emit_session_ready(
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) {
    bootstrap_session(session, ctx).await;

    // Publish the per-agent identity to the runtime-side scheduler
    // registry. The chat WS path itself goes through
    // `dispatch_turn_to_agent`, which already builds the right
    // `AgentLoopConfig` from `Session`. The reason we still register
    // here is the worker fan-out path: when a tool-permission update
    // or post-automaton-completion fan-out triggers
    // `Scheduler::schedule_agent`, the scheduler must be able to
    // build the correct config out-of-band. Without this registration
    // the worker path 429s with `aura_org_id="missing"
    // aura_session_id="missing"`. See
    // `crates/aura-runtime/src/scheduler.rs` for the registry
    // contract and `crates/aura-runtime/src/gateway/handlers/tx.rs` /
    // `crates/aura-runtime/src/gateway/handlers/tool_permissions.rs` for the
    // worker fan-out callers.
    ctx.scheduler
        .identity_registry()
        .register(session.agent_id, session.as_runtime_identity());

    let defaults = match session_user_defaults(session, ctx) {
        Ok(defaults) => defaults,
        Err(e) => {
            error!(
                session_id = %session.session_id,
                error = %e,
                "Failed to load user tool defaults for SessionReady"
            );
            UserToolDefaults::default()
        }
    };
    let tools: Vec<ToolInfo> = effective_tool_infos(session, &defaults);

    let skills: Vec<SkillInfo> = match (&ctx.skill_manager, &session.skill_agent_id) {
        (Some(sm), Some(agent_id)) => {
            if let Ok(mgr) = sm.read() {
                mgr.agent_skill_meta(agent_id)
                    .into_iter()
                    .map(|m| SkillInfo {
                        name: m.name,
                        description: m.description,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };

    info!(
        session_id = %session.session_id,
        model = %session.model,
        tool_count = tools.len(),
        integration_count = session.installed_integrations.len(),
        skill_count = skills.len(),
        "Session initialized"
    );

    let _ = outbound_tx.try_send(OutboundMessage::SessionReady(SessionReady {
        session_id: session.session_id.clone(),
        tools,
        skills,
    }));
}

/// Build the per-session kernel and emit its bootstrap transactions.
///
/// Two side-effecting steps are performed (and three error paths logged
/// via the original `error!` strings so operators don't see message
/// drift):
/// 1. `Transaction::session_start` — Invariant §2 (Every State Change
///    Is a Transaction) + §11 (Session-Scoped Approvals): the session
///    start is itself a state change that must be recorded and that
///    resets the kernel's session-scoped approval cache. Emit the
///    transaction before anything else on this kernel so the record
///    reflects session boundaries for replay.
/// 2. `record_runtime_capabilities` — bundles the `installed_tools` /
///    `installed_integrations` snapshot into a single `System` record
///    entry so downstream replay knows what surface the LLM was given.
///
/// Failures only log; we deliberately don't surface them to the caller
/// because a kernel build / capability-recording failure must NOT
/// prevent us from sending `SessionReady` (the UI would otherwise spin
/// forever waiting on a session that already loaded).
async fn bootstrap_session(session: &Session, ctx: &WsContext) {
    match build_kernel_with_config(session, ctx, &ctx.tool_config, None, None).await {
        Ok(kernel) => {
            if let Err(e) = kernel
                .process_direct(aura_core_types::Transaction::session_start(
                    session.agent_id,
                ))
                .await
            {
                error!(
                    session_id = %session.session_id,
                    error = %e,
                    "Failed to record SessionStart transaction through kernel"
                );
            }

            if let Err(e) = capabilities::record_runtime_capabilities(
                &kernel,
                "session",
                Some(&session.session_id),
                &session.installed_tools,
                &session.installed_integrations,
            )
            .await
            {
                error!(
                    session_id = %session.session_id,
                    error = %e,
                    "Failed to record runtime capability install through kernel during session init"
                );
            }
        }
        Err(e) => {
            error!(
                session_id = %session.session_id,
                error = %e,
                "Failed to build kernel for session capability recording"
            );
        }
    }
}

/// `parent_outbound` + `parent_cancellation` are `Some` only on the
/// per-turn build path. When supplied, the subagent dispatch hook is
/// wrapped in a [`super::subagent_stream::RuntimeSubagentObservabilityHook`]
/// so each `task` child run is announced + streamed on the parent
/// stream and registered as its own WS-attachable thread. The session
/// bootstrap path passes `None` (no active turn to stream onto) and
/// gets the plain blocking dispatcher.
pub(super) async fn build_kernel_with_config(
    session: &Session,
    ctx: &WsContext,
    tool_config: &aura_tools::ToolConfig,
    parent_outbound: Option<&mpsc::Sender<OutboundMessage>>,
    parent_cancellation: Option<&CancellationToken>,
) -> Result<Arc<Kernel>, aura_agent_kernel::KernelError> {
    let domain_exec = ctx.domain_api.as_ref().map(|api| {
        use aura_tools::domain_tools::DomainToolExecutor;
        Arc::new(DomainToolExecutor::with_session_context(
            api.clone(),
            session.auth_token.clone(),
            session.project_id.clone(),
            session
                .project_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        ))
    });

    let user_default = session_user_defaults(session, ctx)?;
    let session_tool_config = session_scoped_tool_config(
        tool_config,
        &user_default,
        session.tool_permissions.as_ref(),
    );

    let mut resolver =
        executor::build_tool_resolver(&ctx.catalog, &session_tool_config, domain_exec.clone())
            .with_installed_tools(session.installed_tools.clone());

    // Register the live computer-use tool only when the run opted into
    // computer-use AND an executor URL is configured. The catalog gates
    // visibility behind `Capability::ComputerUse`; this registers the
    // executable side bound to the per-session executor endpoint.
    if session.computer_use {
        if let Some(url) = session
            .computer_executor_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
        {
            resolver.register(Box::new(aura_tools::ComputerTool::new(url)));
        }
    }

    if let Some(ref controller) = ctx.automaton_controller {
        let project_id = session.project_id.clone().unwrap_or_default();
        let workspace_root = session.project_path.clone();
        for tool in aura_tools::automaton_tools::devloop_control_tools(
            controller.clone(),
            project_id,
            workspace_root,
            session.auth_token.clone(),
        ) {
            resolver.register(tool);
        }
    }

    let (workspace, use_workspace_base_as_root) = resolve_session_workspace(session);

    let mut policy = PolicyConfig::default();
    policy.set_installed_integrations(session.installed_integrations.iter().cloned());
    policy.set_tool_integration_requirements(session.installed_tools.iter().filter_map(|tool| {
        tool.required_integration
            .clone()
            .map(|requirement| (tool.name.clone(), requirement))
    }));
    // Permissions are mandatory on every session; wire them into the
    // kernel policy unconditionally so the Delegate gate enforces them.
    policy.agent_permissions = session.agent_permissions.clone();
    policy = policy
        .with_user_default(user_default.clone())
        .with_agent_override(session.tool_permissions.clone());

    // Per-session fleet subagent dispatcher (fresh registry / quota /
    // leases / orphan store + the engine's `RuntimeChildRunner`). Built
    // by a shared helper so the council orchestrator
    // ([`super::council::start_council_run`]) constructs the identical
    // dispatcher when fanning members out, instead of duplicating the
    // wiring.
    let fleet_dispatcher = build_fleet_subagent_dispatcher(session, ctx)?;
    // On the per-turn path, wrap the blocking dispatcher so each child
    // run is observable as its own live WS thread without regressing the
    // inline `SubagentResult` the parent still receives.
    let dispatch_hook: Arc<dyn aura_tools::SubagentDispatchHook> = match parent_outbound {
        Some(outbound) => Arc::new(
            super::subagent_stream::RuntimeSubagentObservabilityHook::new(
                fleet_dispatcher,
                outbound.clone(),
                ctx.chat_runs.clone(),
                parent_cancellation.cloned(),
                ctx.run_id.clone(),
            ),
        ),
        None => fleet_dispatcher,
    };
    resolver = resolver.with_subagent_dispatch_hook(dispatch_hook);
    if let Some(base_url) = ctx
        .aura_os_server_url
        .as_deref()
        .filter(|url| !url.is_empty())
    {
        resolver = resolver.with_spawn_hook(Arc::new(AuraServerSpawnHook::new(
            base_url.to_string(),
            session.auth_token.clone(),
            session.aura_org_id.clone(),
            ctx.store.clone(),
        )));
        let hook = Arc::new(AuraServerAgentHook::new(
            base_url.to_string(),
            session.auth_token.clone(),
        ));
        resolver = resolver
            .with_agent_control_hook(hook.clone())
            .with_agent_read_hook(hook);
    } else {
        resolver = resolver.with_spawn_hook(Arc::new(aura_agent_kernel::KernelSpawnHook::new(
            ctx.store.clone(),
        )));
    }
    resolver = resolver
        .with_caller_permissions(session.agent_permissions.clone())
        .with_tool_permission_context(user_default, session.tool_permissions.clone())
        .with_originating_user_id(session.user_id.clone());

    // Thread the upstream OS UUID for this session into the tool context
    // so cross-agent tools can ship a server-resolvable caller id as
    // `originating_agent_id` / `parent_agent_id` instead of the truncated
    // harness blake3 hash. `skill_agent_id` is populated from
    // `RuntimeRequest.agent_identity.template_id` (the `aura-os-server`
    // `agents.agent_id` UUID) when present, with a fallback to the raw
    // `agent_id` string — see `SessionState::from_runtime_request` in
    // `state.rs`. Without this wire-up
    // the server-side `spawn_cross_agent_reply_callback` POSTs to
    // `/api/agents/{16_char_hex}/events/stream`, which the
    // `Path<AgentId = Uuid>` extractor rejects with 400 and the async
    // reply chain dies silently.
    if let Some(external_id) = session.skill_agent_id.as_deref() {
        if !external_id.trim().is_empty() {
            resolver = resolver.with_caller_external_agent_id(external_id.to_string());
        }
    }

    // Thread this session's resolved model into the tool context so
    // cross-agent tools (`send_to_agent`, `delegate_task`) forward it
    // to the target agent's turn. Cross-agent recipients usually have
    // no server-side configured model, so without this the recipient's
    // harness session opens with an empty model and the turn fails with
    // "model name must not be empty". Blank model is left unset so the
    // builder keeps the legacy null-model wire behavior.
    if !session.model.trim().is_empty() {
        resolver = resolver.with_caller_model_id(session.model.clone());
    }

    let router = executor::build_executor_router(resolver);

    let config = KernelConfig {
        workspace_base: workspace,
        use_workspace_base_as_root,
        policy,
        tool_approval_prompter: session
            .tool_approval_broker
            .clone()
            .map(|broker| broker as Arc<dyn aura_agent_kernel::ToolApprovalPrompter>),
        originating_user_id: Some(session.user_id.clone()),
        ..KernelConfig::default()
    };

    let kernel = Kernel::new(
        ctx.store.clone(),
        session
            .provider_override
            .clone()
            .unwrap_or_else(|| ctx.provider.clone()),
        router,
        config,
        session.agent_id,
    )?;

    Ok(Arc::new(kernel))
}

/// Build the per-session [`FleetSubagentDispatcher`] (fresh registry /
/// quota / leases / orphan store wired to the engine's
/// [`RuntimeChildRunner`] + the session-equivalent child-kernel
/// factory).
///
/// Extracted from [`build_kernel_with_config`] as the single per-turn
/// subagent dispatch surface. An AURA Council
/// ([`super::council::start_council_run`]) reuses it implicitly: the
/// synthesizer model issues real parallel `task` calls, which dispatch
/// through this exact surface (children inherit the parent identity /
/// permissions / parent-chain and run the full real-agent loop, with
/// only their model overridden per member). The inputs (`domain_exec`,
/// the session-scoped tool config, the resolved workspace) are
/// recomputed here so the helper is self-contained; the duplicated
/// computation is cheap and pure.
pub(super) fn build_fleet_subagent_dispatcher(
    session: &Session,
    ctx: &WsContext,
) -> Result<Arc<FleetSubagentDispatcher>, aura_agent_kernel::KernelError> {
    let domain_exec = ctx.domain_api.as_ref().map(|api| {
        use aura_tools::domain_tools::DomainToolExecutor;
        Arc::new(DomainToolExecutor::with_session_context(
            api.clone(),
            session.auth_token.clone(),
            session.project_id.clone(),
            session
                .project_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        ))
    });

    let user_default = session_user_defaults(session, ctx)?;
    let session_tool_config = session_scoped_tool_config(
        &ctx.tool_config,
        &user_default,
        session.tool_permissions.as_ref(),
    );
    let (workspace, use_workspace_base_as_root) = resolve_session_workspace(session);

    let subagent_registry = SubagentRegistry::bundled();
    // Durable per-node orphan store under the node data dir
    // (`workspace_base` is `<data_dir>/workspaces`, so its parent is the
    // data dir). Detached/abandoned subagents must survive in a real
    // location so `aura agents inspect` can find them — not the OS temp
    // dir, which is volatile and shared across unrelated installs.
    let orphan_dir = ctx.workspace_base.parent().map_or_else(
        || ctx.workspace_base.join("subagent-orphans"),
        |data_dir| data_dir.join("subagent-orphans"),
    );
    // Cross-crate seam: build the session-equivalent child-kernel
    // factory (declared in aura-engine, implemented here) and inject it
    // into the child runner so a child run reuses the real-agent
    // resolver — subagent dispatch, spawn hooks, caller permissions, and
    // parent_chain — instead of the scheduler's bare node resolver. The
    // factory is self-referential (re-injects itself into the children
    // it spawns) so nesting works to arbitrary depth.
    let child_kernel_factory = super::child_kernel::SessionChildKernelFactory::new(
        super::child_kernel::SessionChildKernelFactoryParams {
            catalog: ctx.catalog.clone(),
            session_tool_config: session_tool_config.clone(),
            domain_exec: domain_exec.clone(),
            installed_tools: session.installed_tools.clone(),
            automaton_controller: ctx.automaton_controller.clone(),
            automaton_project_id: session.project_id.clone().unwrap_or_default(),
            automaton_workspace_root: session.project_path.clone(),
            store: ctx.store.clone(),
            scheduler: ctx.scheduler.clone(),
            subagent_registry: subagent_registry.clone(),
            orphan_dir: orphan_dir.clone(),
            workspace: workspace.clone(),
            use_workspace_base_as_root,
            aura_os_server_url: ctx.aura_os_server_url.clone(),
            auth_token: session.auth_token.clone(),
            aura_org_id: session.aura_org_id.clone(),
        },
    );
    // Root child subagents at the parent session's resolved workspace so
    // they can read the same project files the parent sees, rather than
    // the scheduler's empty `workspace_base/<child_id>` scratch dir.
    let child_runner: Arc<dyn aura_fleet_spawn::ChildRunner> = Arc::new(
        RuntimeChildRunner::new(
            ctx.store.clone(),
            ctx.scheduler.clone(),
            subagent_registry.clone(),
        )
        .with_child_workspace(workspace.clone(), use_workspace_base_as_root)
        .with_child_kernel_factory(child_kernel_factory),
    );
    Ok(Arc::new(FleetSubagentDispatcher::with_components(
        ctx.store.clone(),
        subagent_registry,
        Arc::new(FleetRegistry::new()),
        Arc::new(QuotaPool::new()),
        Arc::new(ParentLeaseRegistry::new()),
        Arc::new(OrphanStore::new(orphan_dir)),
        child_runner,
    )))
}

/// [`TurnEventSink`] that maps events onto the WebSocket wire protocol.
///
/// Phase 3 consolidated the handwritten match here and its sibling in
/// the TUI's `UiCommandSink` into a single dispatcher
/// ([`map_agent_loop_event`]). Each sink overrides only the hooks it
/// cares about; unhandled variants fall through to no-op defaults,
/// but the dispatcher's match is exhaustive, so adding a new
/// [`AgentLoopEvent`] variant is still a compile error until every
/// consumer has handled it.
///
/// The WS sink awaits bounded mpsc capacity so transient saturation
/// applies backpressure instead of dropping later terminal frames.
struct OutboundMessageSink<'a> {
    outbound: &'a mpsc::Sender<OutboundMessage>,
    closed: bool,
    pending_delta: Option<PendingStreamDelta>,
}

enum PendingStreamDelta {
    Text(String),
    Thinking(String),
}

impl PendingStreamDelta {
    fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Thinking(thinking) => thinking.len(),
        }
    }

    fn into_message(self) -> OutboundMessage {
        match self {
            Self::Text(text) => OutboundMessage::TextDelta(TextDelta { text }),
            Self::Thinking(thinking) => OutboundMessage::ThinkingDelta(ThinkingDelta { thinking }),
        }
    }
}

impl OutboundMessageSink<'_> {
    async fn push(&mut self, msg: OutboundMessage) {
        self.flush_pending_delta().await;
        self.push_now(msg).await;
    }

    async fn push_text_delta(&mut self, text: String) {
        self.push_delta(StreamDeltaKind::Text, text).await;
    }

    async fn push_thinking_delta(&mut self, thinking: String) {
        self.push_delta(StreamDeltaKind::Thinking, thinking).await;
    }

    async fn push_delta(&mut self, kind: StreamDeltaKind, chunk: String) {
        if self.closed {
            return;
        }

        let matches_pending = matches!(
            (&self.pending_delta, kind),
            (Some(PendingStreamDelta::Text(_)), StreamDeltaKind::Text)
                | (
                    Some(PendingStreamDelta::Thinking(_)),
                    StreamDeltaKind::Thinking
                )
        );
        if self.pending_delta.is_some() && !matches_pending {
            self.flush_pending_delta().await;
        }

        match (&mut self.pending_delta, kind) {
            (Some(PendingStreamDelta::Text(buffer)), StreamDeltaKind::Text) => {
                buffer.push_str(&chunk);
            }
            (Some(PendingStreamDelta::Thinking(buffer)), StreamDeltaKind::Thinking) => {
                buffer.push_str(&chunk);
            }
            (slot @ None, StreamDeltaKind::Text) => {
                *slot = Some(PendingStreamDelta::Text(chunk));
            }
            (slot @ None, StreamDeltaKind::Thinking) => {
                *slot = Some(PendingStreamDelta::Thinking(chunk));
            }
            _ => unreachable!("mismatched pending delta was flushed before append"),
        }

        if self
            .pending_delta
            .as_ref()
            .is_some_and(|delta| delta.len() >= STREAM_DELTA_COALESCE_BYTES)
        {
            self.flush_pending_delta().await;
        }
    }

    async fn flush_pending_delta(&mut self) {
        if self.closed {
            return;
        }
        if let Some(delta) = self.pending_delta.take() {
            self.push_now(delta.into_message()).await;
        }
    }

    async fn push_now(&mut self, msg: OutboundMessage) {
        if self.closed {
            return;
        }
        if !send_outbound_with_backpressure(self.outbound, msg, "turn_event").await {
            self.closed = true;
        }
    }
}

#[derive(Clone, Copy)]
enum StreamDeltaKind {
    Text,
    Thinking,
}

async fn send_outbound_with_backpressure(
    outbound: &mpsc::Sender<OutboundMessage>,
    msg: OutboundMessage,
    context: &'static str,
) -> bool {
    match timeout(OUTBOUND_DELIVERY_TIMEOUT, outbound.send(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => {
            warn!(context, "Outbound channel closed while delivering message");
            false
        }
        Err(_) => {
            warn!(
                context,
                timeout_ms =
                    u64::try_from(OUTBOUND_DELIVERY_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
                "Timed out delivering outbound message"
            );
            false
        }
    }
}

#[async_trait]
impl TurnEventSink for OutboundMessageSink<'_> {
    async fn on_text_delta(&mut self, text: String) {
        self.push_text_delta(text).await;
    }

    async fn on_thinking_delta(&mut self, thinking: String) {
        self.push_thinking_delta(thinking).await;
    }

    async fn on_tool_start(&mut self, id: String, name: String) {
        self.push(OutboundMessage::ToolUseStart(ToolUseStart { id, name }))
            .await;
    }

    async fn on_tool_result(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        image: Option<aura_core_types::ToolResultImage>,
    ) {
        // Split the typed image into the flat wire fields aura-os
        // persists. Never log the base64 payload — only dims/length are
        // safe (the executor already logged dims on capture).
        let (image_base64, image_media_type) = match image {
            Some(img) => (Some(img.base64), Some(img.media_type)),
            None => (None, None),
        };
        self.push(OutboundMessage::ToolResult(ToolResultMsg {
            name: tool_name,
            result: content,
            is_error,
            tool_use_id: Some(tool_use_id),
            image_base64,
            image_media_type,
        }))
        .await;
    }

    async fn on_tool_input_snapshot(&mut self, id: String, name: String, input: String) {
        // While streaming with `eager_input_streaming`, `input` is
        // partial JSON like `{"title":"Hi","markdown_contents":"# H`.
        // A strict `serde_json::from_str` would fail and yield `{}`,
        // making every mid-stream snapshot useless to the UI. Use a
        // tool-aware partial-JSON extractor that pulls out the
        // best-effort value of well-known string fields the preview
        // cards consume (markdown_contents, content, old_text, etc.).
        let parsed = super::partial_json::parse_partial_tool_input(&name, &input);
        let str_field_len =
            |key: &str| parsed.get(key).and_then(|v| v.as_str()).map_or(0, str::len);
        let md_len = str_field_len("markdown_contents");
        let content_len = str_field_len("content");
        let description_len = str_field_len("description");
        tracing::info!(
            tool = %name,
            raw_input_bytes = input.len(),
            parsed_keys = parsed.as_object().map_or(0, |o| o.len()),
            markdown_len = md_len,
            content_len,
            description_len,
            "forwarding tool_call_snapshot"
        );
        self.push(OutboundMessage::ToolCallSnapshot(ToolCallSnapshot {
            id,
            name,
            input: parsed,
        }))
        .await;
    }

    async fn on_error(&mut self, code: String, message: String, recoverable: bool) {
        self.push(OutboundMessage::Error(ErrorMsg {
            code,
            message,
            recoverable,
            support_id: None,
        }))
        .await;
    }

    async fn on_progress(
        &mut self,
        stage: String,
        tool_name: Option<String>,
        elapsed_ms: Option<u64>,
        message: Option<String>,
    ) {
        self.push(OutboundMessage::Progress(crate::protocol::ProgressMsg {
            stage,
            tool_name,
            elapsed_ms,
            message,
        }))
        .await;
    }

    // The following variants are intentional no-ops on the WS wire —
    // `ToolComplete`, `IterationComplete`, `ThinkingComplete`,
    // `StepComplete`, `StreamReset`, `Warning`, `Debug`. The trait
    // defaults cover them, but the mapper's exhaustive match still
    // forces a decision here whenever the event enum changes.
    async fn on_debug(&mut self, _event: DebugEvent) {}
}

pub(super) async fn forward_events_to_ws(
    mut event_rx: mpsc::Receiver<AgentLoopEvent>,
    outbound: mpsc::Sender<OutboundMessage>,
) {
    let mut sink = OutboundMessageSink {
        outbound: &outbound,
        closed: false,
        pending_delta: None,
    };
    while let Some(event) = event_rx.recv().await {
        map_agent_loop_event(event, &mut sink).await;
        if sink.closed {
            break;
        }
    }
    sink.flush_pending_delta().await;
}

pub(super) async fn finalize_turn(
    session: &mut Session,
    join_result: Result<anyhow::Result<AgentLoopResult>, tokio::task::JoinError>,
    message_id: &str,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) {
    let result = match join_result {
        Ok(inner) => inner,
        Err(e) => {
            error!(session_id = %session.session_id, error = %e, "Turn task panicked");
            send_turn_error(outbound_tx, message_id).await;
            return;
        }
    };

    match result {
        Ok(loop_result) => {
            apply_turn_result(session, &loop_result, message_id, outbound_tx).await;
        }
        Err(e) => {
            error!(session_id = %session.session_id, error = %e, "Turn processing failed");
            send_outbound_with_backpressure(
                outbound_tx,
                OutboundMessage::Error(ErrorMsg {
                    code: "turn_error".into(),
                    message: format!("Turn processing failed: {e}"),
                    recoverable: true,
                    support_id: None,
                }),
                "turn_error",
            )
            .await;
        }
    }
}

async fn send_turn_error(outbound_tx: &mpsc::Sender<OutboundMessage>, message_id: &str) {
    send_outbound_with_backpressure(
        outbound_tx,
        OutboundMessage::Error(ErrorMsg {
            code: "internal_error".into(),
            message: "Turn processing task panicked".into(),
            recoverable: false,
            support_id: None,
        }),
        "turn_panic_error",
    )
    .await;
    send_outbound_with_backpressure(
        outbound_tx,
        OutboundMessage::AssistantMessageEnd(Box::new(AssistantMessageEnd {
            message_id: message_id.to_string(),
            stop_reason: "error".into(),
            usage: SessionUsage::default(),
            files_changed: FilesChanged::default(),
            originating_user_id: None,
        })),
        "turn_panic_end",
    )
    .await;
}

pub(super) async fn apply_turn_result(
    session: &mut Session,
    loop_result: &AgentLoopResult,
    message_id: &str,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) {
    session.messages.clone_from(&loop_result.messages);
    // Defense-in-depth: cap oversized tool_use inputs / tool_result content
    // so one large blob does not ride along with every subsequent prompt.
    aura_context_compaction::compact_for_storage(&mut session.messages);
    let files_changed = summarize_files_changed(loop_result);

    let input_tokens = loop_result.total_input_tokens;
    let output_tokens = loop_result.total_output_tokens;
    let estimated_context_tokens = loop_result.estimated_context_tokens;
    let cache_creation_input_tokens = loop_result.total_cache_creation_input_tokens;
    let cache_read_input_tokens = loop_result.total_cache_read_input_tokens;
    session.cumulative_input_tokens += input_tokens;
    session.cumulative_output_tokens += output_tokens;
    session.cumulative_cache_creation_input_tokens += cache_creation_input_tokens;
    session.cumulative_cache_read_input_tokens += cache_read_input_tokens;

    let stop_reason = if loop_result.timed_out {
        "cancelled"
    } else if loop_result.insufficient_credits {
        "insufficient_credits"
    } else if loop_result.llm_error.is_some() {
        "end_turn_with_errors"
    } else {
        "end_turn"
    };

    let context_utilization = if session.context_window_tokens > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = estimated_context_tokens as f32 / session.context_window_tokens as f32;
        ratio.min(1.0)
    } else {
        0.0
    };

    let breakdown = &loop_result.context_breakdown;
    let context_breakdown = ContextBreakdown {
        system_prompt_tokens: breakdown.system_prompt_tokens,
        tools_tokens: breakdown.tools_tokens,
        skills_tokens: breakdown.skills_tokens,
        mcp_tokens: breakdown.mcp_tokens,
        subagents_tokens: breakdown.subagents_tokens,
        conversation_tokens: breakdown.conversation_tokens,
        cache_read_tokens: breakdown.cache_read_tokens,
        cache_creation_tokens: breakdown.cache_creation_tokens,
    };
    let context_contents = build_wire_context_contents(loop_result);

    send_outbound_with_backpressure(
        outbound_tx,
        OutboundMessage::AssistantMessageEnd(Box::new(AssistantMessageEnd {
            message_id: message_id.to_string(),
            stop_reason: stop_reason.into(),
            usage: SessionUsage {
                input_tokens,
                output_tokens,
                estimated_context_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                cumulative_input_tokens: session.cumulative_input_tokens,
                cumulative_output_tokens: session.cumulative_output_tokens,
                cumulative_cache_creation_input_tokens: session
                    .cumulative_cache_creation_input_tokens,
                cumulative_cache_read_input_tokens: session.cumulative_cache_read_input_tokens,
                context_utilization,
                model: session.model.clone(),
                provider: session.provider_name.clone(),
                context_breakdown,
                context_contents,
            },
            files_changed,
            originating_user_id: None,
        })),
        "assistant_message_end",
    )
    .await;

    info!(
        session_id = %session.session_id,
        timed_out = loop_result.timed_out,
        iterations = loop_result.iterations,
        history_len = session.messages.len(),
        "Turn complete"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        apply_turn_result, emit_session_ready, finalize_turn, forward_events_to_ws,
        prepare_chat_session, resolve_session_workspace, session_scoped_tool_config,
        summarize_files_changed,
    };
    use crate::gateway::session::{Session, WsContext};
    use crate::protocol::{OutboundMessage, TextDelta};
    use aura_agent::{AgentLoopEvent, AgentLoopResult, FileChange, FileChangeKind};
    use aura_core_types::{AgentToolPermissions, ToolState, UserToolDefaults};
    use aura_engine::scheduler::Scheduler;
    use aura_model_reasoner::MockProvider;
    use aura_protocol::{
        AgentCapabilities, AgentIdentity, AgentPermissionsWire, ModelSelection, RuntimeRequest,
        RuntimeRequestType, SessionModelOverrides, WorkspaceLocation,
    };
    use aura_store_db::RocksStore;
    use aura_tools::{ToolCatalog, ToolConfig};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::Duration;

    fn test_context() -> WsContext {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let db_dir = tempfile::tempdir().expect("temp db");
        let store = Arc::new(RocksStore::open(db_dir.path(), false).expect("open rocks store"));
        let provider = Arc::new(MockProvider::simple_response("ok"));
        let workspace_base = workspace.path().to_path_buf();
        let catalog = Arc::new(ToolCatalog::default());
        let scheduler = Arc::new(Scheduler::new(
            store.clone(),
            provider.clone(),
            Vec::new(),
            catalog.executor_builtin_tools(),
            workspace_base.clone(),
            None,
        ));
        std::mem::forget(workspace);
        std::mem::forget(db_dir);

        WsContext {
            workspace_base,
            provider,
            store,
            scheduler,
            tool_config: ToolConfig::default(),
            auth_token: None,
            catalog,
            domain_api: None,
            automaton_controller: None,
            project_base: None,
            memory_manager: None,
            skill_manager: None,
            router_url: None,
            aura_os_server_url: None,
            chat_runs: Arc::new(dashmap::DashMap::new()),
            run_id: None,
        }
    }

    #[test]
    fn summarize_files_changed_groups_by_operation() {
        let loop_result = AgentLoopResult {
            file_changes: vec![
                FileChange {
                    path: "src/new.rs".into(),
                    kind: FileChangeKind::Create,
                    lines_added: 0,
                    lines_removed: 0,
                },
                FileChange {
                    path: "src/lib.rs".into(),
                    kind: FileChangeKind::Modify,
                    lines_added: 0,
                    lines_removed: 0,
                },
                FileChange {
                    path: "src/old.rs".into(),
                    kind: FileChangeKind::Delete,
                    lines_added: 0,
                    lines_removed: 0,
                },
            ],
            ..AgentLoopResult::default()
        };

        let summary = summarize_files_changed(&loop_result);
        assert_eq!(summary.created, vec!["src/new.rs"]);
        assert_eq!(summary.modified, vec!["src/lib.rs"]);
        assert_eq!(summary.deleted, vec!["src/old.rs"]);
        // No FileChange carried non-zero counts, so diffs stays empty
        // and the wire format doesn't carry a misleading 0/0 entry.
        assert!(summary.diffs.is_empty());
    }

    #[test]
    fn summarize_files_changed_emits_diffs_for_nonzero_counts() {
        let loop_result = AgentLoopResult {
            file_changes: vec![
                FileChange {
                    path: "src/touched.rs".into(),
                    kind: FileChangeKind::Modify,
                    lines_added: 7,
                    lines_removed: 2,
                },
                // write_file path: counts stay at 0 (unknown), so this
                // entry must NOT show up in diffs.
                FileChange {
                    path: "src/new.rs".into(),
                    kind: FileChangeKind::Create,
                    lines_added: 0,
                    lines_removed: 0,
                },
            ],
            ..AgentLoopResult::default()
        };

        let summary = summarize_files_changed(&loop_result);
        assert_eq!(summary.diffs.len(), 1);
        assert_eq!(summary.diffs[0].path, "src/touched.rs");
        assert_eq!(summary.diffs[0].lines_added, 7);
        assert_eq!(summary.diffs[0].lines_removed, 2);
    }

    #[test]
    fn resolve_session_workspace_uses_project_path_directly() {
        let mut session = Session::new(PathBuf::from("/tmp/aura"));
        session.project_path = Some(PathBuf::from("/tmp/project"));

        let (workspace, use_workspace_base_as_root) = resolve_session_workspace(&session);

        assert_eq!(workspace, PathBuf::from("/tmp/project"));
        assert!(use_workspace_base_as_root);
    }

    #[test]
    fn resolve_session_workspace_uses_explicit_workspace_directly() {
        let mut session = Session::new(PathBuf::from("/tmp/aura"));
        session.workspace = PathBuf::from("/tmp/aura/session-123");

        let (workspace, use_workspace_base_as_root) = resolve_session_workspace(&session);

        assert_eq!(workspace, PathBuf::from("/tmp/aura/session-123"));
        assert!(use_workspace_base_as_root);
    }

    #[test]
    fn resolve_session_workspace_keeps_base_for_default_workspace() {
        let session = Session::new(PathBuf::from("/tmp/aura"));

        let (workspace, use_workspace_base_as_root) = resolve_session_workspace(&session);

        assert_eq!(workspace, PathBuf::from("/tmp/aura"));
        assert!(!use_workspace_base_as_root);
    }

    #[test]
    fn session_config_bypasses_allowlists_only_when_both_gates_open() {
        let mut base = ToolConfig::for_autonomous_dev_loop();
        base.command.allow_unrestricted_full_access = true;

        let full_access = UserToolDefaults::full_access();
        let config = session_scoped_tool_config(&base, &full_access, None);
        assert!(config.command.bypass_allowlists);

        let mut operator_off = base.clone();
        operator_off.command.allow_unrestricted_full_access = false;
        let config = session_scoped_tool_config(&operator_off, &full_access, None);
        assert!(!config.command.bypass_allowlists);

        let config = session_scoped_tool_config(&base, &UserToolDefaults::auto_review(), None);
        assert!(!config.command.bypass_allowlists);

        let narrowing_override = AgentToolPermissions::new().with("run_command", ToolState::Ask);
        let config = session_scoped_tool_config(&base, &full_access, Some(&narrowing_override));
        assert!(!config.command.bypass_allowlists);
    }

    #[tokio::test]
    async fn streamed_text_and_assistant_end_wait_for_outbound_capacity() {
        let (outbound_tx, mut outbound_rx) = mpsc::channel(1);
        outbound_tx
            .send(OutboundMessage::TextDelta(TextDelta {
                text: "backlog".to_string(),
            }))
            .await
            .expect("fill outbound channel");

        let (event_tx, event_rx) = mpsc::channel(1);
        let forward_handle = tokio::spawn(forward_events_to_ws(event_rx, outbound_tx.clone()));
        event_tx
            .send(AgentLoopEvent::TextDelta("hello".to_string()))
            .await
            .expect("queue text event");
        drop(event_tx);

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !forward_handle.is_finished(),
            "stream forwarding should wait while outbound is full"
        );

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "backlog"
        ));
        forward_handle.await.expect("stream forward task joins");

        let mut session = Session::new(PathBuf::from("/tmp/aura"));
        let loop_result = AgentLoopResult::default();
        let apply_tx = outbound_tx.clone();
        let apply_handle = tokio::spawn(async move {
            apply_turn_result(&mut session, &loop_result, "msg-1", &apply_tx).await;
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !apply_handle.is_finished(),
            "assistant end should wait behind the streamed text"
        );

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "hello"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::AssistantMessageEnd(end)) if end.message_id == "msg-1"
        ));
        apply_handle.await.expect("apply task joins");
    }

    #[tokio::test]
    async fn forward_events_coalesces_consecutive_text_deltas() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        let forward_handle = tokio::spawn(forward_events_to_ws(event_rx, outbound_tx));

        event_tx
            .send(AgentLoopEvent::TextDelta("hel".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::TextDelta("lo".to_string()))
            .await
            .unwrap();
        drop(event_tx);

        forward_handle.await.expect("stream forward task joins");

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "hello"
        ));
        assert!(outbound_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn forward_events_flushes_text_before_non_text_event() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        let forward_handle = tokio::spawn(forward_events_to_ws(event_rx, outbound_tx));

        event_tx
            .send(AgentLoopEvent::TextDelta("before".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::ToolStart {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
            })
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::TextDelta("after".to_string()))
            .await
            .unwrap();
        drop(event_tx);

        forward_handle.await.expect("stream forward task joins");

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "before"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::ToolUseStart(start))
                if start.id == "tool-1" && start.name == "read_file"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "after"
        ));
        assert!(outbound_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn forward_events_coalesces_thinking_separately_from_text() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        let forward_handle = tokio::spawn(forward_events_to_ws(event_rx, outbound_tx));

        event_tx
            .send(AgentLoopEvent::ThinkingDelta("think".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::ThinkingDelta("ing".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::TextDelta("text".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::ThinkingDelta("more".to_string()))
            .await
            .unwrap();
        drop(event_tx);

        forward_handle.await.expect("stream forward task joins");

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::ThinkingDelta(delta)) if delta.thinking == "thinking"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "text"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::ThinkingDelta(delta)) if delta.thinking == "more"
        ));
        assert!(outbound_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn forward_events_flushes_pending_text_before_terminal_error() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        let forward_handle = tokio::spawn(forward_events_to_ws(event_rx, outbound_tx));

        event_tx
            .send(AgentLoopEvent::TextDelta("partial".to_string()))
            .await
            .unwrap();
        event_tx
            .send(AgentLoopEvent::Error {
                code: "stream_error".to_string(),
                message: "boom".to_string(),
                recoverable: true,
            })
            .await
            .unwrap();
        drop(event_tx);

        forward_handle.await.expect("stream forward task joins");

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "partial"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::Error(err))
                if err.code == "stream_error" && err.message == "boom" && err.recoverable
        ));
        assert!(outbound_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn finalize_turn_error_waits_for_outbound_capacity() {
        let (outbound_tx, mut outbound_rx) = mpsc::channel(1);
        outbound_tx
            .send(OutboundMessage::TextDelta(TextDelta {
                text: "backlog".to_string(),
            }))
            .await
            .expect("fill outbound channel");

        let panic_handle = tokio::spawn(async {
            panic!("turn panic");
            #[allow(unreachable_code)]
            anyhow::Ok(AgentLoopResult::default())
        });
        let join_result = panic_handle.await;

        let mut session = Session::new(PathBuf::from("/tmp/aura"));
        let finalize_tx = outbound_tx.clone();
        let finalize_handle = tokio::spawn(async move {
            finalize_turn(&mut session, join_result, "msg-2", &finalize_tx).await;
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !finalize_handle.is_finished(),
            "turn error should wait while outbound is full"
        );

        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::TextDelta(delta)) if delta.text == "backlog"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::Error(err)) if err.code == "internal_error"
        ));
        assert!(matches!(
            outbound_rx.recv().await,
            Some(OutboundMessage::AssistantMessageEnd(end))
                if end.message_id == "msg-2" && end.stop_reason == "error"
        ));
        finalize_handle.await.expect("finalize task joins");
    }

    #[test]
    fn agent_loop_config_omits_upstream_provider_family_after_overrides_only_carry_model_knobs() {
        let mut session = Session::new(PathBuf::from("/tmp/aura"));
        session.provider_overrides = Some(SessionModelOverrides {
            default_model: Some("deepseek-v4-flash".to_string()),
            fallback_model: None,
            prompt_caching_enabled: Some(true),
            prompt_cache_key: None,
            prompt_cache_retention: None,
        });

        let config = session.agent_loop_config();

        // The wire `SessionModelOverrides` no longer carries a family
        // hint; family detection on the harness side falls back to the
        // model-name heuristic in `RouterProvider::supports_anthropic_proxy_features`.
        assert_eq!(config.upstream_provider_family, None);
    }

    fn chat_request(
        workspace: Option<String>,
        provider_overrides: Option<SessionModelOverrides>,
    ) -> RuntimeRequest {
        RuntimeRequest {
            r#type: RuntimeRequestType::Chat {
                conversation_messages: Vec::new(),
            },
            agent_identity: AgentIdentity::default(),
            model: ModelSelection {
                provider_overrides,
                ..ModelSelection::default()
            },
            workspace: WorkspaceLocation {
                workspace,
                project_path: None,
                git_repo_url: None,
                git_branch: None,
            },
            project: None,
            agent_permissions: AgentPermissionsWire::default(),
            tool_permissions: None,
            agent_capabilities: AgentCapabilities::default(),
            auth_jwt: None,
            user_id: "user-test".to_string(),
        }
    }

    #[tokio::test]
    async fn failed_init_does_not_leave_provider_override_state() {
        let ctx = test_context();

        let invalid_workspace = tempfile::tempdir()
            .expect("outside workspace")
            .path()
            .join("outside");
        let invalid_request = chat_request(
            Some(invalid_workspace.display().to_string()),
            Some(SessionModelOverrides {
                default_model: Some("claude-opus-4-6".to_string()),
                fallback_model: None,
                prompt_caching_enabled: Some(true),
                prompt_cache_key: None,
                prompt_cache_retention: None,
            }),
        );

        let outcome = prepare_chat_session(invalid_request, &ctx).await;
        let err = match outcome {
            Ok(_) => panic!("workspace outside base must reject"),
            Err(e) => e,
        };
        assert_eq!(err.code, "invalid_workspace");

        let retry_workspace = ctx.workspace_base.join("retry-session");
        std::fs::create_dir_all(&retry_workspace).expect("retry workspace should exist");
        let retry_request = chat_request(Some(retry_workspace.display().to_string()), None);

        let session = prepare_chat_session(retry_request, &ctx)
            .await
            .expect("valid workspace prepares cleanly");
        assert!(session.initialized);
        assert!(session.provider_override.is_none());
    }

    /// Wave 2 T2 — Invariants §2 + §11:
    ///
    /// Bootstrapping a chat session must submit a
    /// `Transaction::session_start(...)` through the kernel so the
    /// record log reflects the session boundary and the policy's
    /// session-scoped approvals are cleared.
    ///
    /// A follow-on kernel call (what `start_turn` now does) must
    /// append a `UserPrompt` entry with the user message as payload.
    #[tokio::test]
    async fn session_bootstrap_emits_session_start_and_user_prompt_are_recorded() {
        use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
        use aura_core_types::{Transaction, TransactionType};

        let ctx = test_context();

        let ws_path = ctx.workspace_base.join("record-test");
        std::fs::create_dir_all(&ws_path).unwrap();

        let request = chat_request(Some(ws_path.display().to_string()), None);

        let mut session = prepare_chat_session(request, &ctx)
            .await
            .expect("valid chat request prepares cleanly");
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        emit_session_ready(&mut session, &outbound_tx, &ctx).await;

        assert!(session.initialized);
        let agent_id = session.agent_id;

        // Drain session_ready so the channel doesn't block downstream asserts.
        let _ = outbound_rx.recv().await;

        // SessionStart must be the first recorded transaction for this agent.
        let entries = ctx.store.scan_record(agent_id, 1, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.tx.tx_type == TransactionType::SessionStart),
            "expected SessionStart entry in record, got: {:?}",
            entries.iter().map(|e| e.tx.tx_type).collect::<Vec<_>>(),
        );

        let kernel = Arc::new(
            Kernel::new(
                ctx.store.clone(),
                ctx.provider.clone(),
                ExecutorRouter::new(),
                KernelConfig {
                    workspace_base: ws_path.clone(),
                    use_workspace_base_as_root: true,
                    ..KernelConfig::default()
                },
                agent_id,
            )
            .unwrap(),
        );
        kernel
            .process_direct(Transaction::user_prompt(agent_id, "hello kernel"))
            .await
            .unwrap();

        let entries = ctx.store.scan_record(agent_id, 1, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.tx.tx_type == TransactionType::UserPrompt
                    && e.tx.payload.as_ref() == b"hello kernel"),
            "expected UserPrompt entry with payload 'hello kernel', got: {:?}",
            entries.iter().map(|e| e.tx.tx_type).collect::<Vec<_>>(),
        );
    }
}
