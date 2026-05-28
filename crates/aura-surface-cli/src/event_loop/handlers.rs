//! Handler functions for agent turns, auth, and session lifecycle.

use super::record_ui::{create_response_transaction, send_record_to_ui};
use super::{forward_agent_events, LoopState, TURN_TIMEOUT};
use aura_agent::AgentLoopEvent;
use aura_core::{SystemKind, Transaction, TransactionType};
use aura_terminal::UiCommand;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub(super) async fn run_agent_turn(
    state: &mut LoopState<'_>,
) -> (
    Result<aura_agent::AgentLoopResult, aura_agent::AgentError>,
    bool,
) {
    let (agent_event_tx, agent_event_rx) = tokio::sync::mpsc::channel::<AgentLoopEvent>(1024);

    let fwd_commands = state.commands.clone();
    let forwarder = tokio::spawn(forward_agent_events(agent_event_rx, fwd_commands));

    let cancel_token = CancellationToken::new();
    let cancel_for_timeout = cancel_token.clone();

    let process_result = match tokio::time::timeout(
        TURN_TIMEOUT,
        state.agent_loop.run_with_events(
            state.model_gateway,
            state.tool_gateway,
            state.messages.clone(),
            state.tools.to_vec(),
            Some(agent_event_tx),
            Some(cancel_token),
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            cancel_for_timeout.cancel();
            Err(aura_agent::AgentError::Timeout(format!(
                "Agent turn timed out after {} minutes",
                TURN_TIMEOUT.as_secs() / 60
            )))
        }
    };

    let streamed_text = match forwarder.await {
        Ok(fwd_state) => {
            if fwd_state.thinking_active {
                let _ = state.commands.send(UiCommand::FinishThinking).await;
            }
            if fwd_state.streaming_active {
                let _ = state.commands.send(UiCommand::FinishStreaming).await;
            }
            fwd_state.had_text
        }
        Err(e) => {
            warn!(error = %e, "Event forwarder panicked");
            false
        }
    };

    (process_result, streamed_text)
}

pub(super) async fn handle_agent_success(
    state: &mut LoopState<'_>,
    result: aura_agent::AgentLoopResult,
    streamed_text: bool,
) {
    state.messages = result.messages.clone();

    persist_response_via_kernel(state, &result.total_text).await;
    emit_response_to_ui(state, &result, streamed_text).await;

    // Memory ingestion now fires automatically via TurnObserver inside
    // AgentLoop::run_with_events — no manual call needed here.

    if let Some(ref err) = result.llm_error {
        let _ = state
            .commands
            .send(UiCommand::ShowWarning(format!("LLM error: {err}")))
            .await;
    }
    if result.timed_out {
        let _ = state
            .commands
            .send(UiCommand::ShowWarning("Agent loop timed out".to_string()))
            .await;
    }
    let _ = state.commands.send(UiCommand::Complete).await;
}

async fn persist_response_via_kernel(state: &mut LoopState<'_>, total_text: &str) {
    let response_tx = create_response_transaction(state.agent_id, total_text);

    match state.kernel.process_direct(response_tx.clone()).await {
        Ok(result) => {
            debug!(
                seq = result.entry.seq,
                "Response record persisted via kernel"
            );
            send_record_to_ui(
                state.commands,
                result.entry.seq,
                &response_tx,
                &result.entry,
            )
            .await;
        }
        Err(e) => {
            error!(error = %e, "Failed to persist response record via kernel");
        }
    }
}

async fn emit_response_to_ui(
    state: &LoopState<'_>,
    result: &aura_agent::AgentLoopResult,
    streamed_text: bool,
) {
    if !result.total_text.is_empty() {
        let preview: String = result.total_text.chars().take(100).collect();
        info!(response_preview = %preview, "Model response received");

        if !streamed_text {
            let _ = state
                .commands
                .send(UiCommand::ShowMessage(aura_terminal::events::MessageData {
                    role: aura_terminal::events::MessageRole::Assistant,
                    content: result.total_text.clone(),
                    is_streaming: false,
                }))
                .await;
        }
    }
}

pub(super) async fn handle_new_session(state: &mut LoopState<'_>) {
    debug!("New session requested");

    let session_tx = Transaction::session_start(state.agent_id);
    let store = state.kernel.store();

    if let Err(e) = store.enqueue_tx(&session_tx) {
        error!(error = %e, "Failed to enqueue session start");
    } else if let Ok(Some((token, tx))) = store.dequeue_tx(state.agent_id) {
        match state.kernel.process_dequeued(tx.clone(), token).await {
            Ok(result) => {
                debug!(
                    seq = result.entry.seq,
                    "Session start record persisted via kernel"
                );
                send_record_to_ui(state.commands, result.entry.seq, &tx, &result.entry).await;
            }
            Err(e) => {
                error!(error = %e, "Failed to persist session start record via kernel");
            }
        }
    }

    state.messages.clear();

    let _ = state
        .commands
        .send(UiCommand::SetStatus("Ready".to_string()))
        .await;
}

pub(super) async fn handle_login(state: &mut LoopState<'_>, email: &str, password: &str) {
    let _ = state
        .commands
        .send(UiCommand::SetStatus("Authenticating...".to_string()))
        .await;
    match aura_auth::ZosClient::new() {
        Ok(client) => match client.login(email, password).await {
            Ok(stored) => {
                let display = stored.display_name.clone();
                let zid = stored.primary_zid.clone();
                let token = stored.access_token.clone();
                if let Err(e) = aura_auth::CredentialStore::save(&stored) {
                    let _ = state
                        .commands
                        .send(UiCommand::ShowError(format!(
                            "Failed to save credentials: {e}"
                        )))
                        .await;
                } else {
                    state.agent_loop.set_auth_token(Some(token));
                    let auth_payload = serde_json::json!({
                        "system_kind": SystemKind::AuthChange,
                        "action": "login",
                        "display_name": display,
                        "zid": zid,
                    });
                    if let Ok(payload_bytes) = serde_json::to_vec(&auth_payload) {
                        let auth_tx = Transaction::new_chained(
                            state.agent_id,
                            TransactionType::System,
                            payload_bytes,
                            None,
                        );
                        if let Err(e) = state.kernel.process_direct(auth_tx).await {
                            warn!(error = %e, "Failed to record auth login via kernel");
                        }
                    } else {
                        warn!("Failed to serialize auth login payload");
                    }
                    let _ = state
                        .commands
                        .send(UiCommand::ShowSuccess(format!(
                            "Logged in as {display} ({zid})"
                        )))
                        .await;
                }
            }
            Err(e) => {
                let _ = state
                    .commands
                    .send(UiCommand::ShowError(format!("Login failed: {e}")))
                    .await;
            }
        },
        Err(e) => {
            let _ = state
                .commands
                .send(UiCommand::ShowError(format!("Auth client error: {e}")))
                .await;
        }
    }
    let _ = state.commands.send(UiCommand::Complete).await;
}

pub(super) async fn handle_logout(state: &mut LoopState<'_>) {
    let session_before = aura_auth::CredentialStore::load();
    if let Some(stored) = session_before.as_ref() {
        if let Ok(client) = aura_auth::ZosClient::new() {
            client.logout(&stored.access_token).await;
        }
    }
    match aura_auth::CredentialStore::clear() {
        Ok(()) => {
            state.agent_loop.set_auth_token(None);
            if let Some(stored) = session_before {
                let auth_payload = serde_json::json!({
                    "system_kind": SystemKind::AuthChange,
                    "action": "logout",
                    "display_name": stored.display_name,
                    "zid": stored.primary_zid,
                });
                if let Ok(payload_bytes) = serde_json::to_vec(&auth_payload) {
                    let auth_tx = Transaction::new_chained(
                        state.agent_id,
                        TransactionType::System,
                        payload_bytes,
                        None,
                    );
                    if let Err(e) = state.kernel.process_direct(auth_tx).await {
                        warn!(error = %e, "Failed to record auth logout via kernel");
                    }
                } else {
                    warn!("Failed to serialize auth logout payload");
                }
            }
            let _ = state
                .commands
                .send(UiCommand::ShowSuccess("Logged out".to_string()))
                .await;
        }
        Err(e) => {
            let _ = state
                .commands
                .send(UiCommand::ShowError(format!(
                    "Failed to clear credentials: {e}"
                )))
                .await;
        }
    }
}

pub(super) async fn handle_whoami(state: &LoopState<'_>) {
    match aura_auth::CredentialStore::load() {
        Some(session) => {
            let msg = format!(
                "Logged in as {} (zID: {}, User: {}, Since: {})",
                session.display_name,
                session.primary_zid,
                session.user_id,
                session.created_at.format("%Y-%m-%d %H:%M UTC"),
            );
            let _ = state
                .commands
                .send(UiCommand::ShowMessage(aura_terminal::events::MessageData {
                    role: aura_terminal::events::MessageRole::System,
                    content: msg,
                    is_streaming: false,
                }))
                .await;
        }
        None => {
            let _ = state
                .commands
                .send(UiCommand::ShowMessage(aura_terminal::events::MessageData {
                    role: aura_terminal::events::MessageRole::System,
                    content: "Not logged in. Use /login to authenticate.".to_string(),
                    is_streaming: false,
                }))
                .await;
        }
    }
}
