//! WebSocket connection handler and turn management.

use super::generation::{self, GenerationTurn};
use super::helpers;
use super::{Session, ToolApprovalBroker, WsContext};
use crate::protocol::{
    AssistantMessageStart, ErrorMsg, InboundMessage, OutboundMessage, UserMessage,
};
use aura_agent::{
    AgentLoop, AgentLoopEvent, AgentLoopResult, KernelModelGateway, KernelToolGateway,
};
use aura_reasoner::{ContentBlock, ImageSource, Message, Role};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use base64::Engine;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// State for a turn that is currently being processed in the background.
enum ActiveTurn {
    Agent(AgentTurn),
    Generation(GenerationTurn),
}

struct AgentTurn {
    cancel_token: CancellationToken,
    join_handle: JoinHandle<anyhow::Result<AgentLoopResult>>,
    stream_forward_handle: JoinHandle<()>,
    message_id: String,
}

/// Classification of a raw WebSocket frame.
enum WsAction {
    Message(String),
    Close,
    Continue,
}

/// Classify a raw WebSocket receive result.
fn classify_ws_frame(msg_result: Option<Result<WsMessage, axum::Error>>) -> WsAction {
    match msg_result {
        Some(Ok(WsMessage::Text(text))) => WsAction::Message(text),
        Some(Ok(WsMessage::Close(_)) | Err(_)) | None => WsAction::Close,
        Some(Ok(_)) => WsAction::Continue,
    }
}

/// Handle a WebSocket connection through its full lifecycle.
pub async fn handle_ws_connection(socket: WebSocket, ctx: WsContext) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundMessage>(1024);

    let send_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    let message_type = outbound_message_type(&json);
                    if ws_tx.send(WsMessage::Text(json)).await.is_err() {
                        warn!(
                            %message_type,
                            "WebSocket outbound send failed; client likely disconnected"
                        );
                        break;
                    }
                }
                Err(e) => error!(error = %e, "Failed to serialize outbound message"),
            }
        }
    });

    let mut session = Session::new(ctx.workspace_base.clone());
    session.auth_token = ctx.auth_token.clone();
    session.project_base = ctx.project_base.clone();
    session.tool_approval_broker = Some(Arc::new(ToolApprovalBroker::new(outbound_tx.clone())));
    info!(session_id = %session.session_id, "WebSocket connection opened");

    let mut active_turn: Option<ActiveTurn> = None;

    loop {
        if let Some(ref mut turn) = active_turn {
            let action = run_active_turn_select(&mut ws_rx, turn, &mut session, &outbound_tx).await;
            match action {
                TurnAction::TurnFinished => {
                    active_turn = None;
                }
                TurnAction::Close => break,
                TurnAction::Continue => {}
            }
        } else {
            match run_idle_select(&mut ws_rx, &mut session, &outbound_tx, &ctx).await {
                IdleAction::StartTurn(turn) => active_turn = Some(turn),
                IdleAction::Close => break,
                IdleAction::Continue => {}
            }
        }
    }

    info!(session_id = %session.session_id, "WebSocket connection closed");
    drop(outbound_tx);
    let _ = send_task.await;
}

fn outbound_message_type(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|ty| ty.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

enum TurnAction {
    TurnFinished,
    Close,
    Continue,
}

async fn run_active_turn_select(
    ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    turn: &mut ActiveTurn,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) -> TurnAction {
    match turn {
        ActiveTurn::Agent(agent) => {
            tokio::select! {
                biased;
                msg_result = ws_rx.next() => {
                    match classify_ws_frame(msg_result) {
                        WsAction::Message(raw) => {
                            handle_msg_during_turn(&raw, &agent.cancel_token, session, outbound_tx);
                            TurnAction::Continue
                        }
                        WsAction::Close => {
                            debug!(session_id = %session.session_id, "Client closed during active turn");
                            agent.cancel_token.cancel();
                            TurnAction::Close
                        }
                        WsAction::Continue => TurnAction::Continue,
                    }
                }
                join_result = &mut agent.join_handle => {
                    let message_id = agent.message_id.clone();
                    agent.stream_forward_handle.abort();
                    helpers::finalize_turn(session, join_result, &message_id, outbound_tx);
                    TurnAction::TurnFinished
                }
            }
        }
        ActiveTurn::Generation(gen) => {
            tokio::select! {
                biased;
                msg_result = ws_rx.next() => {
                    match classify_ws_frame(msg_result) {
                        WsAction::Message(raw) => {
                            handle_msg_during_turn(&raw, &gen.cancel_token, session, outbound_tx);
                            TurnAction::Continue
                        }
                        WsAction::Close => {
                            info!(session_id = %session.session_id, "Client closed during generation");
                            gen.cancel_token.cancel();
                            TurnAction::Close
                        }
                        WsAction::Continue => TurnAction::Continue,
                    }
                }
                join_result = &mut gen.join_handle => {
                    match join_result {
                        Ok(()) => {
                            info!(
                                session_id = %session.session_id,
                                "Generation turn task finished"
                            );
                        }
                        Err(e) => {
                            error!(
                                session_id = %session.session_id,
                                error = %e,
                                "Generation turn task failed"
                            );
                        }
                    }
                    TurnAction::TurnFinished
                }
            }
        }
    }
}

fn handle_msg_during_turn(
    raw: &str,
    cancel_token: &CancellationToken,
    session: &Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) {
    match serde_json::from_str::<InboundMessage>(raw) {
        Ok(InboundMessage::Cancel) => {
            info!(session_id = %session.session_id, "Cancelling active turn");
            cancel_token.cancel();
        }
        Ok(InboundMessage::ToolApprovalResponse(resp)) => {
            if let Some(ref broker) = session.tool_approval_broker {
                if let Err(e) = broker.respond(resp) {
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "approval_response_error".into(),
                        message: e,
                        recoverable: true,
                    }));
                }
            }
        }
        Ok(_) => {
            let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                code: "turn_in_progress".into(),
                message: "A turn is currently in progress; send cancel first".into(),
                recoverable: true,
            }));
        }
        Err(e) => {
            let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                code: "parse_error".into(),
                message: format!("Invalid message: {e}"),
                recoverable: true,
            }));
        }
    }
}

enum IdleAction {
    StartTurn(ActiveTurn),
    Close,
    Continue,
}

async fn run_idle_select(
    ws_rx: &mut futures_util::stream::SplitStream<WebSocket>,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) -> IdleAction {
    match classify_ws_frame(ws_rx.next().await) {
        WsAction::Message(raw) => dispatch_idle_message(&raw, session, outbound_tx, ctx).await,
        WsAction::Close => {
            debug!(session_id = %session.session_id, "Client sent close frame");
            IdleAction::Close
        }
        WsAction::Continue => IdleAction::Continue,
    }
}

async fn dispatch_idle_message(
    raw: &str,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) -> IdleAction {
    match serde_json::from_str::<InboundMessage>(raw) {
        Ok(InboundMessage::SessionInit(init)) => {
            helpers::handle_session_init(session, *init, outbound_tx, ctx).await;
            IdleAction::Continue
        }
        Ok(InboundMessage::UserMessage(msg)) => {
            match start_turn(session, msg, outbound_tx, ctx).await {
                Some(turn) => IdleAction::StartTurn(ActiveTurn::Agent(turn)),
                None => IdleAction::Continue,
            }
        }
        Ok(InboundMessage::GenerationRequest(req)) => {
            info!(
                session_id = %session.session_id,
                mode = %req.mode,
                has_router_url = ctx.router_url.is_some(),
                "Generation request received"
            );
            match ctx.router_url {
                Some(ref url) => match generation::start_generation(session, req, outbound_tx, url)
                {
                    Some(turn) => IdleAction::StartTurn(ActiveTurn::Generation(turn)),
                    None => IdleAction::Continue,
                },
                None => {
                    warn!(
                        session_id = %session.session_id,
                        mode = %req.mode,
                        "Generation request rejected because AURA_ROUTER_URL is not configured"
                    );
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "no_router_url".into(),
                        message: "AURA_ROUTER_URL not configured; generation unavailable".into(),
                        recoverable: false,
                    }));
                    IdleAction::Continue
                }
            }
        }
        Ok(InboundMessage::Cancel) => {
            debug!(session_id = %session.session_id, "Cancel received but no turn is active");
            IdleAction::Continue
        }
        Ok(InboundMessage::ApprovalResponse(resp)) => {
            debug!(
                session_id = %session.session_id,
                tool_use_id = %resp.tool_use_id,
                approved = resp.approved,
                "Approval response received (not yet implemented)"
            );
            IdleAction::Continue
        }
        Ok(InboundMessage::ToolApprovalResponse(resp)) => {
            if let Some(ref broker) = session.tool_approval_broker {
                if let Err(e) = broker.respond(resp) {
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "approval_response_error".into(),
                        message: e,
                        recoverable: true,
                    }));
                }
            }
            IdleAction::Continue
        }
        Err(e) => {
            let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                code: "parse_error".into(),
                message: format!("Invalid message: {e}"),
                recoverable: true,
            }));
            IdleAction::Continue
        }
    }
}

pub(super) fn populate_tool_definitions(session: &mut Session, ctx: &WsContext) {
    let user_default = ctx
        .store
        .get_user_tool_defaults(&session.user_id)
        .ok()
        .flatten()
        .unwrap_or_default();
    session.tool_definitions = crate::tool_permissions::effective_tool_definitions(
        &ctx.catalog,
        &ctx.tool_config,
        &session.installed_tools,
        &session.installed_integrations,
        &user_default,
        session.tool_permissions.as_ref(),
        Some(&session.agent_permissions),
    )
    .into_iter()
    .map(|(definition, _state)| definition)
    .collect();
}

/// Prepared turn-context — the raw materials [`dispatch_turn_to_agent`]
/// needs to spawn the agent loop. `model_gateway` and `tool_gateway` are
/// already wired to the per-turn kernel; `messages` and `tools` are
/// owned snapshots so the spawned task does not borrow `Session`.
struct PreparedTurn {
    model_gateway: KernelModelGateway,
    tool_gateway: KernelToolGateway,
    config: aura_agent::AgentLoopConfig,
    messages: Vec<Message>,
    tools: Vec<aura_reasoner::ToolDefinition>,
}

/// Prepare and spawn an agent-loop turn as a background task.
async fn start_turn(
    session: &mut Session,
    msg: UserMessage,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) -> Option<AgentTurn> {
    if !session.initialized {
        let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
            code: "not_initialized".into(),
            message: "Send session_init before user_message".into(),
            recoverable: true,
        }));
        return None;
    }

    let message_id = Uuid::new_v4().to_string();
    let _ = outbound_tx.try_send(OutboundMessage::AssistantMessageStart(
        AssistantMessageStart {
            message_id: message_id.clone(),
        },
    ));

    let prepared = match prepare_turn_context(session, msg, ctx).await {
        Ok(p) => p,
        Err(err) => {
            let _ = outbound_tx.try_send(OutboundMessage::Error(err));
            return None;
        }
    };

    Some(dispatch_turn_to_agent(prepared, outbound_tx, message_id))
}

/// Build everything the agent loop needs for a single turn:
/// the user `Message` (with optional image attachments), a session-scoped
/// `tool_config` extended with skill grants, the per-turn kernel and its
/// gateways, and the populated `AgentLoopConfig` (system prompt, skills,
/// memory observer).
///
/// Side-effects, in the order the original inline code performed them:
/// 1. `session.messages.push(user_msg)` — append before any potential
///    failure, so the conversation log reflects the user's intent even
///    if the kernel build subsequently errors.
/// 2. `Transaction::user_prompt` — Invariant §2: every user-visible
///    state change must be a transaction committed through the kernel.
///    Record the prompt before the agent loop runs so the record log
///    reflects the turn boundary even if the loop aborts mid-stream.
///
/// Returns `Err(ErrorMsg)` for the two failure modes the caller surfaces
/// over the WebSocket: kernel build failure and user-prompt recording
/// failure. Error codes / messages match the pre-refactor strings exactly.
async fn prepare_turn_context(
    session: &mut Session,
    msg: UserMessage,
    ctx: &WsContext,
) -> Result<PreparedTurn, ErrorMsg> {
    let user_msg = if let Some(ref attachments) = msg.attachments {
        let image_atts: Vec<_> = attachments.iter().filter(|a| a.type_ == "image").collect();
        if image_atts.is_empty() {
            Message::user(&msg.content)
        } else {
            let mut blocks: Vec<ContentBlock> = Vec::new();
            if !msg.content.is_empty() {
                blocks.push(ContentBlock::text(&msg.content));
            }
            for att in &image_atts {
                let image_data = if let Some(ref url) = att.source_url {
                    if att.data.is_empty() {
                        match fetch_attachment_data(url).await {
                            Ok(data) => data,
                            Err(e) => {
                                warn!(url = %url, error = %e, "Failed to fetch attachment from URL, skipping");
                                continue;
                            }
                        }
                    } else {
                        att.data.clone()
                    }
                } else {
                    att.data.clone()
                };
                blocks.push(ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: att.media_type.clone(),
                        data: image_data,
                    },
                });
            }
            Message::new(Role::User, blocks)
        }
    } else {
        Message::user(&msg.content)
    };
    session.messages.push(user_msg);

    let mut tool_config = ctx.tool_config.clone();
    let has_sm = ctx.skill_manager.is_some();
    let has_aid = session.skill_agent_id.is_some();
    info!(
        session_id = %session.session_id,
        has_skill_manager = has_sm,
        has_skill_agent_id = has_aid,
        skill_agent_id = ?session.skill_agent_id,
        "Skill permission check starting"
    );
    if let (Some(ref sm), Some(ref agent_id)) = (&ctx.skill_manager, &session.skill_agent_id) {
        if let Ok(mgr) = sm.read() {
            let perms = mgr.agent_permissions(agent_id);
            info!(
                session_id = %session.session_id,
                extra_paths = ?perms.extra_paths,
                extra_commands = ?perms.extra_commands,
                "Skill permissions resolved"
            );
            if !perms.extra_paths.is_empty() {
                tool_config.extra_allowed_paths.extend(perms.extra_paths);
            }
            if !perms.extra_commands.is_empty() && !tool_config.command.command_allowlist.is_empty()
            {
                tool_config
                    .command
                    .command_allowlist
                    .extend(perms.extra_commands);
            }
        }
    }

    let kernel = match helpers::build_kernel_with_config(session, ctx, &tool_config).await {
        Ok(k) => k,
        Err(e) => {
            error!(session_id = %session.session_id, error = %e, "Failed to build kernel");
            return Err(ErrorMsg {
                code: "kernel_error".into(),
                message: format!("Failed to build kernel: {e}"),
                recoverable: true,
            });
        }
    };

    if let Err(e) = kernel
        .process_direct(aura_core::Transaction::user_prompt(
            session.agent_id,
            msg.content.clone(),
        ))
        .await
    {
        error!(
            session_id = %session.session_id,
            error = %e,
            "Failed to record UserPrompt transaction through kernel"
        );
        return Err(ErrorMsg {
            code: "kernel_error".into(),
            message: format!("Failed to record user prompt: {e}"),
            recoverable: true,
        });
    }

    let model_gateway = KernelModelGateway::new(kernel.clone());
    let tool_gateway = KernelToolGateway::new(kernel);

    let mut config = session.agent_loop_config();
    config.tool_hints = msg.tool_hints;

    // Resolve active skill names before creating the memory observer so we can
    // forward them for procedure extraction.
    let mut active_skill_names: Vec<String> = Vec::new();
    if let (Some(ref sm), Some(ref agent_id)) = (&ctx.skill_manager, &session.skill_agent_id) {
        if let Ok(mgr) = sm.read() {
            let injected = mgr.inject_agent_skills(agent_id, &mut config.system_prompt);
            if !injected.is_empty() {
                active_skill_names = injected.iter().map(|s| s.name.clone()).collect();
                debug!(
                    session_id = %session.session_id,
                    skill_count = active_skill_names.len(),
                    "Injected agent skills into prompt"
                );
            }
        }
    }
    if let Some(ref mm) = ctx.memory_manager {
        let mem_id = session.memory_agent_id();
        mm.prepare_context(mem_id, &mut config).await;
        config.observers.push(mm.turn_observer_with_skills(
            mem_id,
            session.auth_token.clone(),
            active_skill_names,
        ));
    }

    let tools = session.tool_definitions.clone();
    let messages = session.messages.clone();

    Ok(PreparedTurn {
        model_gateway,
        tool_gateway,
        config,
        messages,
        tools,
    })
}

/// Spawn the agent-loop background task and the matching event-forwarder
/// task, returning the [`AgentTurn`] handle the WebSocket loop polls on.
///
/// Cancellation: a single `CancellationToken` is cloned three times —
/// the original (returned in `AgentTurn`), one moved into the loop
/// future (so cooperative cancellation reaches the model call), and one
/// kept for the post-loop check that flips `timed_out = true` when the
/// run was cancelled rather than completing on its own.
fn dispatch_turn_to_agent(
    prepared: PreparedTurn,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    message_id: String,
) -> AgentTurn {
    let PreparedTurn {
        model_gateway,
        tool_gateway,
        config,
        messages,
        tools,
    } = prepared;
    let agent_loop = AgentLoop::new(config);

    let cancel_token = CancellationToken::new();
    let cancel_for_loop = cancel_token.clone();
    let cancel_for_check = cancel_token.clone();

    let (event_tx, event_rx) = mpsc::channel::<AgentLoopEvent>(1024);

    let join_handle = tokio::spawn(async move {
        let mut result: anyhow::Result<AgentLoopResult> = agent_loop
            .run_with_events(
                &model_gateway,
                &tool_gateway,
                messages,
                tools,
                Some(event_tx),
                Some(cancel_for_loop),
            )
            .await
            .map_err(Into::into);

        if cancel_for_check.is_cancelled() {
            if let Ok(ref mut r) = result {
                r.timed_out = true;
            }
        }
        result
    });

    let outbound_for_stream = outbound_tx.clone();
    let stream_forward_handle =
        tokio::spawn(helpers::forward_events_to_ws(event_rx, outbound_for_stream));

    AgentTurn {
        cancel_token,
        join_handle,
        stream_forward_handle,
        message_id,
    }
}

/// Fetch attachment content from a URL (e.g. S3) and return as base64.
///
/// Only HTTPS URLs are accepted to prevent fetching from arbitrary sources.
async fn fetch_attachment_data(url: &str) -> Result<String, String> {
    if !url.starts_with("https://") {
        return Err("Only HTTPS URLs are allowed".into());
    }
    let bytes = reqwest::get(url)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use aura_core::{
        AgentPermissions, Capability, InstalledIntegrationDefinition, InstalledToolDefinition,
        InstalledToolIntegrationRequirement, ToolAuth,
    };
    use aura_reasoner::MockProvider;
    use aura_store::RocksStore;
    use aura_tools::{ToolCatalog, ToolConfig};
    use std::collections::HashMap;
    use std::sync::Arc;

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
        }
    }

    fn gated_tool() -> InstalledToolDefinition {
        InstalledToolDefinition {
            name: "brave_search_web".to_string(),
            description: "Search the web with Brave".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            endpoint: "https://example.com/tool".to_string(),
            auth: ToolAuth::None,
            timeout_ms: None,
            namespace: None,
            required_integration: Some(InstalledToolIntegrationRequirement {
                integration_id: None,
                provider: Some("brave_search".to_string()),
                kind: Some("workspace_integration".to_string()),
            }),
            runtime_execution: None,
            metadata: HashMap::new(),
        }
    }

    fn brave_integration() -> InstalledIntegrationDefinition {
        InstalledIntegrationDefinition {
            integration_id: "integration-brave-1".to_string(),
            name: "Brave Search".to_string(),
            provider: "brave_search".to_string(),
            kind: "workspace_integration".to_string(),
            metadata: HashMap::new(),
        }
    }

    const CROSS_AGENT_TOOLS: [&str; 7] = [
        "send_to_agent",
        "spawn_agent",
        "agent_lifecycle",
        "get_agent_state",
        "list_agents",
        "delegate_task",
        "task",
    ];

    fn cross_agent_tool_names(session: &Session) -> Vec<String> {
        session
            .tool_definitions
            .iter()
            .filter(|tool| CROSS_AGENT_TOOLS.contains(&tool.name.as_str()))
            .map(|tool| tool.name.clone())
            .collect()
    }

    fn assert_cross_agent_tools(session: &Session, expected: &[&str]) {
        let mut actual = cross_agent_tool_names(session);
        actual.sort();
        let mut expected = expected
            .iter()
            .map(|tool| (*tool).to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn populate_tool_definitions_hides_integration_backed_tool_without_install() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.installed_tools.push(gated_tool());

        populate_tool_definitions(&mut session, &ctx);

        assert!(!session
            .tool_definitions
            .iter()
            .any(|tool| tool.name == "brave_search_web"));
    }

    #[test]
    fn populate_tool_definitions_keeps_integration_backed_tool_with_install() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.installed_tools.push(gated_tool());
        session.installed_integrations.push(brave_integration());

        populate_tool_definitions(&mut session, &ctx);

        assert!(session
            .tool_definitions
            .iter()
            .any(|tool| tool.name == "brave_search_web"));
    }

    #[test]
    fn populate_tool_definitions_includes_ceo_cross_agent_tools() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.agent_permissions = AgentPermissions::ceo_preset();

        populate_tool_definitions(&mut session, &ctx);

        assert_cross_agent_tools(&session, &CROSS_AGENT_TOOLS);
    }

    #[test]
    fn populate_tool_definitions_filters_cross_agent_tools_for_control_agent() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.agent_permissions = AgentPermissions {
            scope: Default::default(),
            capabilities: vec![Capability::ControlAgent],
        };

        populate_tool_definitions(&mut session, &ctx);

        assert_cross_agent_tools(
            &session,
            &["send_to_agent", "agent_lifecycle", "delegate_task"],
        );
    }

    #[test]
    fn populate_tool_definitions_filters_cross_agent_tools_for_spawn_agent() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.agent_permissions = AgentPermissions {
            scope: Default::default(),
            capabilities: vec![Capability::SpawnAgent],
        };

        populate_tool_definitions(&mut session, &ctx);

        assert_cross_agent_tools(&session, &["spawn_agent", "task"]);
    }

    #[test]
    fn populate_tool_definitions_filters_cross_agent_tools_for_read_agent() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.agent_permissions = AgentPermissions {
            scope: Default::default(),
            capabilities: vec![Capability::ReadAgent],
        };

        populate_tool_definitions(&mut session, &ctx);

        assert_cross_agent_tools(&session, &["get_agent_state"]);
    }

    #[test]
    fn populate_tool_definitions_filters_cross_agent_tools_for_list_agents() {
        let ctx = test_context();
        let mut session = Session::new(ctx.workspace_base.clone());
        session.agent_permissions = AgentPermissions {
            scope: Default::default(),
            capabilities: vec![Capability::ListAgents],
        };

        populate_tool_definitions(&mut session, &ctx);

        assert_cross_agent_tools(&session, &["list_agents"]);
    }
}
