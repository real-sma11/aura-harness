//! Chat run driver and turn management.
//!
//! Part C: the chat turn loop is decoupled from any single WebSocket.
//! [`run_chat_driver`] owns the [`Session`] and runs the turn loop in a
//! background task spawned by `POST /v1/run` (see
//! [`super::chat_run::spawn_chat_run`]). Inbound commands arrive over an
//! mpsc fed by the WS adapter ([`super::chat_run::handle_chat_ws_attach`]);
//! outbound frames are emitted into a replay-aware
//! [`super::chat_run::ChatEventChannel`] so a dropped server↔harness
//! socket can reattach (history replay + live) without killing the turn.
//!
//! Two behavior changes vs the pre-Part-C one-shot WS handler:
//! - A WS close NEVER cancels the active turn; it only ends that attach.
//!   Cancellation is explicit (`InboundMessage::Cancel`) or via run stop
//!   (the driver's `shutdown` token).
//! - WS framing/transport errors are handled per-attach by the adapter
//!   and are not broadcast as `Error` frames to other attached clients.

use super::chat_run::{ChatEventChannel, CHAT_RUN_IDLE_RETENTION};
use super::generation::{self, GenerationTurn};
use super::helpers;
use super::{Session, ToolApprovalBroker, WsContext};
use crate::protocol::{
    AssistantMessageStart, ErrorMsg, InboundMessage, OutboundMessage, UserMessage,
};
use aura_agent::{
    AgentLoop, AgentLoopEvent, AgentLoopResult, KernelModelGateway, KernelToolGateway,
};
use aura_model_reasoner::{ContentBlock, ImageSource, Message, Role};
use base64::Engine;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
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

/// Drive a chat run's full turn lifecycle, decoupled from any WS.
///
/// `session` is already populated by [`super::prepare_chat_session`].
/// The driver attaches the approval broker (wired to the run's event
/// channel), emits `SessionReady` into the replay history, then loops:
/// it consumes [`InboundMessage`]s from `commands_rx` (fed by every WS
/// adapter) and emits outbound frames into `outbound_tx` (which the
/// forwarder fans out to history + live subscribers).
///
/// Lifecycle (plan Part C decisions ii/v): the run survives WS drops;
/// it is reaped only when fully idle (no active turn) with zero
/// attached clients for [`CHAT_RUN_IDLE_RETENTION`], or on explicit
/// stop via `shutdown`.
pub(super) async fn run_chat_driver(
    mut session: Session,
    ctx: WsContext,
    events: Arc<ChatEventChannel>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    mut commands_rx: mpsc::Receiver<InboundMessage>,
    attach_count: Arc<AtomicUsize>,
    shutdown: CancellationToken,
) {
    session.tool_approval_broker = Some(Arc::new(ToolApprovalBroker::new(outbound_tx.clone())));
    info!(session_id = %session.session_id, "Chat run driver started");

    // Bootstrap kernel-side state (SessionStart transaction + capability
    // install record + scheduler identity registration) and emit
    // `SessionReady` into the event channel so every (re)attaching
    // client replays it before live traffic.
    helpers::emit_session_ready(&mut session, &outbound_tx, &ctx).await;

    let mut active_turn: Option<ActiveTurn> = None;

    loop {
        if let Some(ref mut turn) = active_turn {
            let action = drive_active_turn(
                &mut commands_rx,
                turn,
                &mut session,
                &outbound_tx,
                &shutdown,
            )
            .await;
            match action {
                TurnAction::TurnFinished => active_turn = None,
                TurnAction::Close => break,
                TurnAction::Continue => {}
            }
        } else {
            match drive_idle(
                &mut commands_rx,
                &mut session,
                &outbound_tx,
                &ctx,
                &attach_count,
                &shutdown,
            )
            .await
            {
                IdleAction::StartTurn(turn) => active_turn = Some(turn),
                IdleAction::Close => break,
                IdleAction::Continue => {}
            }
        }
    }

    // Teardown: cancel any in-flight turn so its background task stops
    // promptly, then mark the channel done so a late attach replays
    // history without waiting on a live receiver that will never fire.
    match active_turn {
        Some(ActiveTurn::Agent(agent)) => agent.cancel_token.cancel(),
        Some(ActiveTurn::Generation(gen)) => gen.cancel_token.cancel(),
        None => {}
    }
    events.mark_done();
    info!(session_id = %session.session_id, "Chat run driver stopped");
    drop(outbound_tx);
}

enum TurnAction {
    TurnFinished,
    Close,
    Continue,
}

/// Advance an active turn: react to one inbound command, the turn's
/// own completion, or an explicit shutdown.
///
/// Crucially, a closed command channel (`recv()` → `None`, i.e. every
/// WS adapter dropped and the run is being torn down) does NOT cancel
/// the turn here; the only cancellation paths are an explicit
/// `InboundMessage::Cancel` or `shutdown`.
async fn drive_active_turn(
    commands_rx: &mut mpsc::Receiver<InboundMessage>,
    turn: &mut ActiveTurn,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    shutdown: &CancellationToken,
) -> TurnAction {
    match turn {
        ActiveTurn::Agent(agent) => {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    info!(session_id = %session.session_id, "Chat run stopped during agent turn; cancelling turn");
                    agent.cancel_token.cancel();
                    TurnAction::Close
                }
                cmd = commands_rx.recv() => {
                    match cmd {
                        Some(msg) => {
                            handle_msg_during_turn(msg, &agent.cancel_token, session, outbound_tx);
                            TurnAction::Continue
                        }
                        // Command channel fully closed: registry entry
                        // removed AND every attach gone. Close the run
                        // (post-loop teardown cancels the turn).
                        None => TurnAction::Close,
                    }
                }
                join_result = &mut agent.join_handle => {
                    complete_agent_turn(join_result, agent, session, outbound_tx).await;
                    TurnAction::TurnFinished
                }
            }
        }
        ActiveTurn::Generation(gen) => {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    info!(session_id = %session.session_id, "Chat run stopped during generation; cancelling turn");
                    gen.cancel_token.cancel();
                    TurnAction::Close
                }
                cmd = commands_rx.recv() => {
                    match cmd {
                        Some(msg) => {
                            handle_msg_during_turn(msg, &gen.cancel_token, session, outbound_tx);
                            TurnAction::Continue
                        }
                        None => TurnAction::Close,
                    }
                }
                join_result = &mut gen.join_handle => {
                    log_generation_join(join_result, session);
                    TurnAction::TurnFinished
                }
            }
        }
    }
}

/// Await the agent turn's stream-forward task and finalize the turn
/// (emit `AssistantMessageEnd`, persist usage / messages).
async fn complete_agent_turn(
    join_result: Result<anyhow::Result<AgentLoopResult>, tokio::task::JoinError>,
    agent: &mut AgentTurn,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) {
    let message_id = agent.message_id.clone();
    if let Err(e) = (&mut agent.stream_forward_handle).await {
        error!(
            session_id = %session.session_id,
            error = %e,
            "Stream forward task failed"
        );
    }
    helpers::finalize_turn(session, join_result, &message_id, outbound_tx).await;
}

fn log_generation_join(join_result: Result<(), tokio::task::JoinError>, session: &Session) {
    match join_result {
        Ok(()) => info!(session_id = %session.session_id, "Generation turn task finished"),
        Err(e) => error!(
            session_id = %session.session_id,
            error = %e,
            "Generation turn task failed"
        ),
    }
}

fn handle_msg_during_turn(
    msg: InboundMessage,
    cancel_token: &CancellationToken,
    session: &Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) {
    match msg {
        InboundMessage::Cancel => {
            info!(session_id = %session.session_id, "Cancelling active turn");
            cancel_token.cancel();
        }
        InboundMessage::ToolApprovalResponse(resp) => {
            if let Some(ref broker) = session.tool_approval_broker {
                if let Err(e) = broker.respond(resp) {
                    crate::inbound_console::ws_rejection_line(
                        "framing",
                        "approval_response_error",
                        Some(&format!("session={} {e}", session.session_id)),
                    );
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "approval_response_error".into(),
                        message: e,
                        recoverable: true,
                        support_id: None,
                    }));
                }
            }
        }
        _ => {
            crate::inbound_console::ws_rejection_line(
                "framing",
                "turn_in_progress",
                Some(&format!("session={}", session.session_id)),
            );
            let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                code: "turn_in_progress".into(),
                message: "A turn is currently in progress; send cancel first".into(),
                recoverable: true,
                support_id: None,
            }));
        }
    }
}

enum IdleAction {
    StartTurn(ActiveTurn),
    Close,
    Continue,
}

/// Wait (idle) for the next inbound command, an explicit shutdown, or
/// the idle-retention deadline.
///
/// When the run sits idle with no command for [`CHAT_RUN_IDLE_RETENTION`],
/// it is reaped only if no client is currently attached (`attach_count
/// == 0`); otherwise the wait re-arms so an attached-but-quiet client
/// keeps the run alive (plan Part C decision ii).
async fn drive_idle(
    commands_rx: &mut mpsc::Receiver<InboundMessage>,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
    attach_count: &Arc<AtomicUsize>,
    shutdown: &CancellationToken,
) -> IdleAction {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            info!(session_id = %session.session_id, "Chat run stopped while idle");
            IdleAction::Close
        }
        result = tokio::time::timeout(CHAT_RUN_IDLE_RETENTION, commands_rx.recv()) => {
            match result {
                Ok(Some(msg)) => dispatch_idle_message(msg, session, outbound_tx, ctx).await,
                // Command channel closed (registry entry removed and all
                // attaches gone): nothing can drive this run further.
                Ok(None) => IdleAction::Close,
                Err(_elapsed) => {
                    if attach_count.load(Ordering::Acquire) == 0 {
                        info!(
                            session_id = %session.session_id,
                            "Chat run idle with no attached clients past retention; reaping"
                        );
                        IdleAction::Close
                    } else {
                        IdleAction::Continue
                    }
                }
            }
        }
    }
}

async fn dispatch_idle_message(
    msg: InboundMessage,
    session: &mut Session,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) -> IdleAction {
    match msg {
        InboundMessage::UserMessage(msg) => {
            match start_turn(session, msg, outbound_tx, ctx).await {
                Some(turn) => IdleAction::StartTurn(ActiveTurn::Agent(turn)),
                None => IdleAction::Continue,
            }
        }
        InboundMessage::GenerationRequest(req) => {
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
                    crate::inbound_console::ws_rejection_line(
                        "framing",
                        "no_router_url",
                        Some(&format!("session={} mode={}", session.session_id, req.mode)),
                    );
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "no_router_url".into(),
                        message: "AURA_ROUTER_URL not configured; generation unavailable".into(),
                        recoverable: false,
                        support_id: None,
                    }));
                    IdleAction::Continue
                }
            }
        }
        InboundMessage::Cancel => {
            debug!(session_id = %session.session_id, "Cancel received but no turn is active");
            IdleAction::Continue
        }
        InboundMessage::ApprovalResponse(resp) => {
            debug!(
                session_id = %session.session_id,
                tool_use_id = %resp.tool_use_id,
                approved = resp.approved,
                "Approval response received (not yet implemented)"
            );
            IdleAction::Continue
        }
        InboundMessage::ToolApprovalResponse(resp) => {
            if let Some(ref broker) = session.tool_approval_broker {
                if let Err(e) = broker.respond(resp) {
                    crate::inbound_console::ws_rejection_line(
                        "framing",
                        "approval_response_error",
                        Some(&format!("session={} {e}", session.session_id)),
                    );
                    let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                        code: "approval_response_error".into(),
                        message: e,
                        recoverable: true,
                        support_id: None,
                    }));
                }
            }
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
    tools: Vec<aura_model_reasoner::ToolDefinition>,
}

/// Prepare and spawn an agent-loop turn as a background task.
async fn start_turn(
    session: &mut Session,
    msg: UserMessage,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    ctx: &WsContext,
) -> Option<AgentTurn> {
    // Phase A: the chat WS is only attached after
    // `prepare_chat_session` has already applied the
    // [`aura_protocol::RuntimeRequest`] on the HTTP side, so
    // `session.initialized` is always true here. The previous
    // "not_initialized" error path (which guarded a missing
    // `session_init` first frame) is unreachable under the new
    // two-step exchange.
    let message_id = Uuid::new_v4().to_string();
    let _ = outbound_tx.try_send(OutboundMessage::AssistantMessageStart(
        AssistantMessageStart {
            message_id: message_id.clone(),
        },
    ));

    // Mint the per-turn cancellation token up front so it can be both
    // threaded into the subagent dispatch hook (built during kernel
    // construction) and used to drive the agent loop below. This makes
    // `SpawnRequest.cancellation` carry the parent token so cancelling
    // the parent turn propagates into a `Wait` child.
    let cancel_token = CancellationToken::new();

    let prepared = match prepare_turn_context(session, msg, ctx, outbound_tx, &cancel_token).await {
        Ok(p) => p,
        Err(err) => {
            crate::inbound_console::ws_rejection_line(
                "framing",
                &err.code,
                Some(&format!("session={} {}", session.session_id, err.message)),
            );
            let _ = outbound_tx.try_send(OutboundMessage::Error(err));
            return None;
        }
    };

    Some(dispatch_turn_to_agent(
        prepared,
        outbound_tx,
        message_id,
        cancel_token,
    ))
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
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    cancel_token: &CancellationToken,
) -> Result<PreparedTurn, ErrorMsg> {
    let user_msg = if let Some(ref attachments) = msg.attachments {
        // Image AND text attachments both reach the model. The previous
        // image-only filter silently dropped text files (`.md`, `.json`,
        // `.sql`, source code from `@`-mentions, etc.), so the agent
        // saw bare text and reported "no file attached". Text payloads
        // are inlined as a `[File: name]\n\n<contents>` text block so
        // the model has a clear content boundary — same shape the
        // frontend uses for the optimistic chat preview, see
        // `interface/src/hooks/attachment-helpers.ts`.
        let usable_atts: Vec<_> = attachments
            .iter()
            .filter(|a| a.type_ == "image" || a.type_ == "text")
            .collect();
        if usable_atts.is_empty() {
            Message::user(&msg.content)
        } else {
            let mut blocks: Vec<ContentBlock> = Vec::new();
            if !msg.content.is_empty() {
                blocks.push(ContentBlock::text(&msg.content));
            }
            for att in &usable_atts {
                // Pull the base64 payload from inline `data` first,
                // falling back to a fetch from `source_url` (S3) when
                // the sender chose the URL-only path.
                let payload_b64 = if let Some(ref url) = att.source_url {
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

                if att.type_ == "image" {
                    blocks.push(ContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".into(),
                            media_type: att.media_type.clone(),
                            data: payload_b64,
                        },
                    });
                } else {
                    let decoded = match base64::engine::general_purpose::STANDARD
                        .decode(&payload_b64)
                    {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(name = ?att.name, error = %e, "Skipping non-UTF-8 text attachment");
                                continue;
                            }
                        },
                        Err(e) => {
                            warn!(name = ?att.name, error = %e, "Skipping text attachment with invalid base64");
                            continue;
                        }
                    };
                    let header = att.name.as_deref().unwrap_or("document");
                    blocks.push(ContentBlock::text(format!("[File: {header}]\n\n{decoded}")));
                }
            }
            if blocks.is_empty() {
                // Every attachment failed to materialize (decode error /
                // S3 fetch failure). Don't ship an empty message —
                // fall back to the bare text so the turn still goes.
                Message::user(&msg.content)
            } else {
                Message::new(Role::User, blocks)
            }
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

    let kernel = match helpers::build_kernel_with_config(
        session,
        ctx,
        &tool_config,
        Some(outbound_tx),
        Some(cancel_token),
    )
    .await
    {
        Ok(k) => k,
        Err(e) => {
            error!(session_id = %session.session_id, error = %e, "Failed to build kernel");
            return Err(ErrorMsg {
                code: "kernel_error".into(),
                message: format!("Failed to build kernel: {e}"),
                recoverable: true,
                support_id: None,
            });
        }
    };

    if let Err(e) = kernel
        .process_direct(aura_core_types::Transaction::user_prompt(
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
            support_id: None,
        });
    }

    let model_gateway = KernelModelGateway::new(kernel.clone());
    let tool_gateway = KernelToolGateway::new(kernel);

    let mut config = session.agent_loop_config();
    config.tool_hints = msg.tool_hints;

    // Subagent surface is constant for the bundled registry today. The
    // dispatch tool re-emits the registry's name + description per
    // entry on every turn, so the per-turn breakdown attributes those
    // chars to the "Subagents" bucket. Custom registries (e.g. tests)
    // would override this once they're plumbed through `WsContext`.
    let subagent_registry = aura_agent_subagent::registry::SubagentRegistry::bundled();
    config.subagents_chars = aura_agent_subagent::registry::registry_chars(&subagent_registry);
    // Parallel per-subagent text for the context-contents viewer: same
    // registry surface `registry_chars` counts tokens for.
    config.subagents_segments =
        aura_agent_subagent::registry::registry_segments(&subagent_registry);

    // Resolve active skill names before creating the memory observer so we can
    // forward them for procedure extraction.
    let mut active_skill_names: Vec<String> = Vec::new();
    if let (Some(ref sm), Some(ref agent_id)) = (&ctx.skill_manager, &session.skill_agent_id) {
        if let Ok(mgr) = sm.read() {
            // Capture the prompt length before injection so the "Skills"
            // bucket can be reported as the delta — the text the
            // skill-injection step actually appends to the system
            // prompt — without double-counting it under "System
            // prompt" downstream in `agent_loop::context::recompute_breakdown`.
            let pre_inject_chars = config.system_prompt.len();
            let injected = mgr.inject_agent_skills(agent_id, &mut config.system_prompt);
            let post_inject_chars = config.system_prompt.len();
            config.skills_chars = post_inject_chars.saturating_sub(pre_inject_chars);
            // Parallel per-skill text for the context-contents viewer.
            // The skill body is already embedded in the system prompt;
            // the "Skills" bucket carries only each skill's name +
            // summary so the text is not double-counted across buckets.
            config.skills_segments = injected
                .iter()
                .map(|s| (s.name.clone(), s.description.clone()))
                .collect();
            if !injected.is_empty() {
                active_skill_names = injected.iter().map(|s| s.name.clone()).collect();
                debug!(
                    session_id = %session.session_id,
                    skill_count = active_skill_names.len(),
                    skills_chars = config.skills_chars,
                    "Injected agent skills into prompt"
                );
            }
        }
    }
    if let Some(ref mm) = ctx.memory_manager {
        let mem_id = session.memory_agent_id();
        mm.prepare_context_with_query(
            mem_id,
            &mut config.system_prompt,
            aura_context_memory::MemoryQueryContext {
                text: msg.content.clone(),
                active_skills: active_skill_names.clone(),
                ..Default::default()
            },
        )
        .await;
        config.observers.push(
            aura_engine::memory_observer::MemoryTurnObserver::new_with_request_context(
                Arc::clone(mm),
                mem_id,
                aura_context_memory::RefinementRequestContext {
                    auth_token: session.auth_token.clone(),
                    aura_project_id: session.project_id.clone(),
                    aura_agent_id: session.aura_agent_id.clone(),
                    aura_session_id: session.aura_session_id.clone(),
                    aura_org_id: session.aura_org_id.clone(),
                },
                active_skill_names,
                Some(session.session_id.clone()),
            ),
        );
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
    cancel_token: CancellationToken,
) -> AgentTurn {
    let PreparedTurn {
        model_gateway,
        tool_gateway,
        config,
        messages,
        tools,
    } = prepared;
    let agent_loop = AgentLoop::new(config);

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
    use aura_core_types::{
        AgentPermissions, Capability, InstalledIntegrationDefinition, InstalledToolDefinition,
        InstalledToolIntegrationRequirement, ToolAuth,
    };
    use aura_engine::scheduler::Scheduler;
    use aura_model_reasoner::MockProvider;
    use aura_store_db::RocksStore;
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
            chat_runs: Arc::new(dashmap::DashMap::new()),
            run_id: None,
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
