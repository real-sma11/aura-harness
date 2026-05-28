//! WebSocket upgrade handlers for the gateway.
//!
//! Phase A note: the two pre-refactor paths `/stream` (chat WS with
//! `SessionInit` first frame) and `/stream/automaton/:id` (event-only
//! automaton stream) collapse into a single `/stream/:run_id` route.
//! Disambiguation happens by run id: chat runs sit in
//! [`super::RouterState::pending_chat_runs`]; automaton runs are
//! tracked by the [`crate::automaton_bridge::AutomatonBridge`].

use super::*;
use axum::response::Response;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum number of concurrent WebSocket connections this node will
/// serve at once. Each live socket holds a tokio task plus terminal /
/// session state; capping the count bounds the "slow-client task
/// exhaustion" worst case flagged by the H5 audit finding.
pub(super) const MAX_WS_CONNS_PER_NODE: usize = 128;

/// Try to reserve a WebSocket connection slot.
pub(super) fn try_acquire_ws_slot(sem: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(sem).try_acquire_owned().ok()
}

/// `WS /stream/:run_id` — bidirectional for chat runs created via
/// `POST /v1/run` with `RuntimeRequestType::Chat`; event-only for
/// DevLoop / TaskRun automaton runs.
///
/// Belt-and-suspenders bearer check (only when
/// [`crate::config::NodeConfig::require_auth`] is on). The router
/// middleware already rejects unauthenticated upgrades; this inline
/// guard prevents a regression if a future contributor wires the
/// handler up to a fresh `Router` that does not inherit the
/// middleware layer.
pub(super) async fn run_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    State(state): State<RouterState>,
) -> Response {
    if state.config.require_auth {
        if let Err(status) = crate::auth::check_bearer(&headers, &state.config.auth_token) {
            crate::inbound_console::ws_rejection_line(
                "upgrade.run",
                "unauthorized",
                Some(&format!("run_id={run_id}")),
            );
            return status.into_response();
        }
    }

    let Some(permit) = try_acquire_ws_slot(&state.ws_slots) else {
        warn!(
            cap = MAX_WS_CONNS_PER_NODE,
            run_id = %run_id,
            "Refusing /stream/:run_id upgrade: WS connection cap reached"
        );
        crate::inbound_console::ws_rejection_line(
            "upgrade.run",
            "slot_full",
            Some(&format!("cap={MAX_WS_CONNS_PER_NODE} run_id={run_id}")),
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let auth_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(String::from);

    // Phase A dispatch: chat runs land in `pending_chat_runs`,
    // automaton runs are looked up via the bridge. Chat sessions
    // take priority — the run-id allocator never collides because
    // chat runs use a freshly minted UUID and automaton runs use
    // their own (also UUID-shaped) ids; if a future regression let
    // an overlap slip through, the chat branch takes the run.
    if let Some(entry) = state.pending_chat_runs.remove(&run_id) {
        let (_key, session_slot) = entry;
        let session = match session_slot.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let Some(session) = session else {
            warn!(run_id = %run_id, "chat run slot already taken");
            crate::inbound_console::ws_rejection_line(
                "upgrade.run",
                "already_attached",
                Some(&format!("run_id={run_id}")),
            );
            return StatusCode::CONFLICT.into_response();
        };
        let ctx = crate::session::WsContext::from_state(&state, auth_token);
        return ws
            .on_upgrade(move |socket| async move {
                crate::session::handle_chat_ws_connection(socket, session, ctx).await;
                drop(permit);
            })
            .into_response();
    }

    let bridge = match state.automaton_bridge.clone() {
        Some(b) => b,
        None => {
            crate::inbound_console::ws_rejection_line(
                "upgrade.run",
                "not_found",
                Some(&format!("run_id={run_id}")),
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    ws.on_upgrade(move |socket| async move {
        handle_automaton_ws(socket, run_id, bridge).await;
        drop(permit);
    })
    .into_response()
}

async fn handle_automaton_ws(
    socket: axum::extract::ws::WebSocket,
    automaton_id: String,
    bridge: Arc<AutomatonBridge>,
) {
    use axum::extract::ws::Message as WsMessage;
    use futures_util::{SinkExt, StreamExt};

    let (mut ws_tx, mut ws_rx) = socket.split();

    let subscription = match bridge.subscribe_events(&automaton_id) {
        Some(sub) => sub,
        None => {
            let msg = serde_json::json!({"type": "error", "message": format!("run {automaton_id} not found or already finished")}).to_string();
            let _: Result<(), _> = ws_tx.send(WsMessage::Text(msg)).await;
            return;
        }
    };
    let crate::automaton_bridge::EventSubscription {
        history,
        mut live,
        already_done,
    } = subscription;

    info!(
        automaton_id = %automaton_id,
        history_len = history.len(),
        already_done,
        "Run event stream connected"
    );

    let drain_aid = automaton_id.clone();
    let drain_handle = tokio::spawn(async move {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        tracing::debug!(automaton_id = %drain_aid, "Run WS read side closed");
    });

    let mut saw_done_in_history = false;
    for event in history {
        let is_done = matches!(event, aura_automaton::AutomatonEvent::Done);
        match serde_json::to_string(&event) {
            Ok(json) => {
                if ws_tx.send(WsMessage::Text(json)).await.is_err() {
                    drain_handle.abort();
                    info!(automaton_id = %automaton_id, "Run event stream disconnected");
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize replayed automaton event");
            }
        }
        if is_done {
            saw_done_in_history = true;
            break;
        }
    }

    if !saw_done_in_history && !already_done {
        loop {
            match live.recv().await {
                Ok(event) => {
                    let is_done = matches!(event, aura_automaton::AutomatonEvent::Done);
                    match serde_json::to_string(&event) {
                        Ok(json) => {
                            if ws_tx.send(WsMessage::Text(json)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to serialize automaton event");
                        }
                    }
                    if is_done {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let msg = serde_json::json!({"type": "warning", "message": format!("dropped {n} events (client too slow)")});
                    let _ = ws_tx.send(WsMessage::Text(msg.to_string())).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    drain_handle.abort();
    info!(automaton_id = %automaton_id, "Run event stream disconnected");
}
