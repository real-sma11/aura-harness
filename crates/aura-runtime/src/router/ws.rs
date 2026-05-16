use super::*;
use axum::response::Response;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum number of concurrent WebSocket connections this node will
/// serve at once, across `/ws/terminal`, `/stream`, and
/// `/stream/automaton/:id`. Each live socket holds a tokio task plus
/// terminal/session state; capping the count bounds the "slow-client
/// task exhaustion" worst case flagged by the H5 audit finding.
///
/// Enforced via [`try_acquire_ws_slot`] against the
/// `Arc<Semaphore>` stored on [`super::RouterState::ws_slots`]. An
/// upgrade handler that fails to acquire a permit returns
/// `503 Service Unavailable` instead of spawning another socket task.
///
/// This cap is *global* — per-IP limiting would require plumbing the
/// peer socket address through every upgrade path. `tower_governor`
/// can't cover long-lived WS sessions because it only inspects the
/// upgrade request. Phase 9 leaves the per-IP cap as a TODO.
pub(super) const MAX_WS_CONNS_PER_NODE: usize = 128;

/// Try to reserve a WebSocket connection slot.
///
/// Returns `Some(permit)` on success — the caller must attach the
/// permit to the spawned socket task so the slot stays held for the
/// lifetime of the connection. Returns `None` when the semaphore is
/// full, in which case the handler should short-circuit with
/// `503 Service Unavailable`.
pub(super) fn try_acquire_ws_slot(sem: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(sem).try_acquire_owned().ok()
}

/// Upgrade an HTTP connection to a WebSocket for interactive agent sessions.
pub(super) async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<RouterState>,
) -> Response {
    let Some(permit) = try_acquire_ws_slot(&state.ws_slots) else {
        warn!(
            cap = MAX_WS_CONNS_PER_NODE,
            "Refusing /stream upgrade: WS connection cap reached"
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let auth_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(String::from);

    let ctx = WsContext {
        workspace_base: state.config.workspaces_path(),
        provider: state.provider.clone(),
        store: state.store.clone(),
        scheduler: state.scheduler.clone(),
        tool_config: state.tool_config.clone(),
        auth_token,
        catalog: state.catalog.clone(),
        domain_api: state.domain_api.clone(),
        automaton_controller: state.automaton_controller.clone(),
        project_base: state.config.project_base.clone(),
        memory_manager: state.memory_manager.clone(),
        skill_manager: state.skill_manager.clone(),
        router_url: state.router_url.clone(),
        aura_os_server_url: state.config.aura_os_server_url.clone(),
    };
    ws.on_upgrade(move |socket| async move {
            // Hold the permit for the lifetime of the socket task so
            // the slot only frees up when the client actually leaves.
            handle_ws_connection(socket, ctx).await;
            drop(permit);
        })
        .into_response()
}

/// WebSocket endpoint for streaming automaton events.
///
/// Clients connect to `/stream/automaton/:automaton_id` to receive real-time
/// events from a running automaton (dev loop, task run, etc.). Requires a
/// non-empty Bearer token in the Authorization header — the prior
/// implementation parsed the token and then dropped it, which was
/// effectively anonymous. (Wave 5 / T1.4.)
pub(super) async fn automaton_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Path(automaton_id): Path<String>,
    State(state): State<RouterState>,
) -> Response {
    // Belt-and-suspenders: the router-wide `require_bearer_mw` middleware
    // (see `router::create_router`) has already rejected callers without
    // a valid Bearer header by the time we get here. Keeping the inline
    // check guards against accidental regressions — e.g. someone wiring
    // this handler up to a fresh `Router` that doesn't inherit the
    // middleware layer. Cost is a single `HeaderMap::get` on an already
    // authenticated path.
    //
    // Only runs when `require_auth` is `true`; when auth is disabled
    // we skip the check so unauthenticated clients can upgrade to the
    // automaton stream (matching the relaxed HTTP routes).
    if state.config.require_auth {
        if let Err(status) = crate::auth::check_bearer(&headers, &state.config.auth_token) {
            return status.into_response();
        }
    }

    let Some(permit) = try_acquire_ws_slot(&state.ws_slots) else {
        warn!(
            cap = MAX_WS_CONNS_PER_NODE,
            automaton_id = %automaton_id,
            "Refusing /stream/automaton/:id upgrade: WS connection cap reached"
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    ws.on_upgrade(move |socket| async move {
            handle_automaton_ws(socket, automaton_id, state.automaton_bridge).await;
            drop(permit);
        })
        .into_response()
}

async fn handle_automaton_ws(
    socket: axum::extract::ws::WebSocket,
    automaton_id: String,
    bridge: Option<Arc<AutomatonBridge>>,
) {
    use axum::extract::ws::Message as WsMessage;
    use futures_util::{SinkExt, StreamExt};

    let (mut ws_tx, mut ws_rx) = socket.split();

    let bridge = match bridge {
        Some(b) => b,
        None => {
            let msg =
                serde_json::json!({"type": "error", "message": "automaton controller unavailable"})
                    .to_string();
            let _: Result<(), _> = ws_tx.send(WsMessage::Text(msg)).await;
            return;
        }
    };

    let subscription = match bridge.subscribe_events(&automaton_id) {
        Some(sub) => sub,
        None => {
            let msg = serde_json::json!({"type": "error", "message": format!("automaton {automaton_id} not found or already finished")}).to_string();
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
        "Automaton event stream connected"
    );

    // Drain the read side so the WebSocket layer can process ping/pong
    // and close frames. Without this the connection may be dropped by
    // intermediaries that expect pong responses.
    let drain_aid = automaton_id.clone();
    let drain_handle = tokio::spawn(async move {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        tracing::debug!(automaton_id = %drain_aid, "Automaton WS read side closed");
    });

    // Phase 1: flush the replay history so a late subscriber
    // (typical: aura-os-server connects after POST /automaton/start
    // returns, by which point a fast-failing automaton has often
    // already emitted every event) sees the full, in-order event
    // sequence any early subscriber would have received. If `Done`
    // is in the history we're free to return as soon as we've sent
    // it - no more events will arrive.
    let mut saw_done_in_history = false;
    for event in history {
        let is_done = matches!(event, aura_automaton::AutomatonEvent::Done);
        match serde_json::to_string(&event) {
            Ok(json) => {
                if ws_tx.send(WsMessage::Text(json)).await.is_err() {
                    drain_handle.abort();
                    info!(automaton_id = %automaton_id, "Automaton event stream disconnected");
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

    // Phase 2: if Done is already past, short-circuit. Otherwise pump
    // the live broadcast for any events emitted after our subscribe.
    // Lagged events are reported to the client but do not terminate
    // the stream; Closed means the retention window elapsed or the
    // bridge shut down.
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
    info!(automaton_id = %automaton_id, "Automaton event stream disconnected");
}
