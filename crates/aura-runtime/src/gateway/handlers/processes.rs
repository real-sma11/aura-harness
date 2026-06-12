//! Process / automation endpoints (Swarm TEE upgrade phase 7).
//!
//! CRUD + trigger over the in-TEE [`aura_store_db::ProcessStore`]:
//!
//! * `GET    /v1/processes`            — list full definitions.
//! * `POST   /v1/processes`            — create (validates cron).
//! * `GET    /v1/processes/:id`        — fetch one.
//! * `PUT    /v1/processes/:id`        — partial update (incl. enable/disable).
//! * `DELETE /v1/processes/:id`        — delete definition + run history.
//! * `POST   /v1/processes/:id/trigger`— execute NOW: starts a chat run
//!   through the same internals as `POST /v1/run` (Chat) and returns
//!   `202 Accepted` with the new run record.
//! * `GET    /v1/processes/:id/runs`   — capped run history, newest first.
//!
//! All routes sit on the protected sub-router behind the same bearer
//! middleware as every other gateway endpoint; this API is only
//! reachable in-VM or via the authenticated control-plane proxy, so
//! the list/get responses may include the prompt/config. The data
//! still never leaves the VM through any *export* path — the only
//! exportable view is [`aura_store_db::ProcessStore::trigger_metadata`]
//! (`process_id`, `cron`, `enabled`, `next_run_at`), which the next
//! phase registers with the swarm gateway.
//!
//! # Trigger execution
//!
//! `POST /v1/processes/:id/trigger` reuses the existing chat run path
//! end-to-end: [`prepare_chat_session`] builds a session from a
//! synthetic `RuntimeRequestType::Chat` request, [`spawn_chat_run`]
//! registers the driver task under the process-run id, and the process
//! prompt is enqueued as the run's first user message. A watcher task
//! subscribes to the run's event channel and marks the
//! [`aura_store_db::ProcessRunRecord`] success/failure when the turn
//! completes, then tears the one-shot run down.

use super::super::*;
use crate::gateway::session::chat_run::ChatRunHandle;
use crate::gateway::session::{prepare_chat_session, spawn_chat_run, WsContext};
use crate::protocol::{InboundMessage, OutboundMessage, UserMessage};
use aura_protocol::{RuntimeRequest, RuntimeRequestType};
use aura_store_db::{
    NewProcess, ProcessError, ProcessRunRecord, ProcessRunStatus, ProcessStore, ProcessUpdate,
};
use std::sync::Arc;

/// Ceiling on how long the watcher waits for a triggered run to finish
/// before recording a failure and tearing the run down.
const TRIGGER_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Cap on the stored success summary assembled from the run's text
/// output.
const RUN_SUMMARY_MAX_CHARS: usize = 500;

/// Map a [`ProcessError`] to an HTTP status + JSON failure body.
fn process_error_response(err: &ProcessError) -> (StatusCode, Json<serde_json::Value>) {
    let status = match err {
        ProcessError::InvalidName(_)
        | ProcessError::InvalidCron(_)
        | ProcessError::InvalidPrompt(_) => StatusCode::BAD_REQUEST,
        ProcessError::NotFound(_) | ProcessError::RunNotFound(_) => StatusCode::NOT_FOUND,
        ProcessError::Store(_) | ProcessError::Serde(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": err.to_string() })),
    )
}

/// 503 response when the node was built without a process store (test
/// fixtures that pass `process_store: None`); production always wires
/// one.
fn store_unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "ok": false, "error": "process store unavailable" })),
    )
        .into_response()
}

fn not_found() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": "process not found" })),
    )
        .into_response()
}

/// Phase 8: after a successful process mutation, fire a best-effort
/// background sync of the trigger-metadata set to the swarm gateway.
/// Never blocks or fails the user's call; no-op for local agents.
fn notify_trigger_registrar(state: &RouterState) {
    if let Some(registrar) = &state.trigger_registrar {
        registrar.sync();
    }
}

/// `GET /v1/processes` — full definitions (in-VM / authenticated proxy
/// consumers only; nothing here is an off-VM export path).
pub(in crate::gateway) async fn list_processes_handler(
    State(state): State<RouterState>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    match store.list() {
        Ok(processes) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "processes": processes })),
        )
            .into_response(),
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `POST /v1/processes` — create. Invalid cron / name / prompt → 400.
pub(in crate::gateway) async fn create_process_handler(
    State(state): State<RouterState>,
    Json(body): Json<NewProcess>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    match store.create(body) {
        Ok(process) => {
            notify_trigger_registrar(&state);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "ok": true, "process": process })),
            )
                .into_response()
        }
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `GET /v1/processes/:id`.
pub(in crate::gateway) async fn get_process_handler(
    State(state): State<RouterState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    match store.get(&id) {
        Ok(Some(process)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "process": process })),
        )
            .into_response(),
        Ok(None) => not_found(),
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `PUT /v1/processes/:id` — partial update; omitted fields keep their
/// stored values, `enabled` toggles scheduling.
pub(in crate::gateway) async fn update_process_handler(
    State(state): State<RouterState>,
    Path(id): Path<String>,
    Json(body): Json<ProcessUpdate>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    match store.update(&id, body) {
        Ok(process) => {
            notify_trigger_registrar(&state);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "process": process })),
            )
                .into_response()
        }
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `DELETE /v1/processes/:id` — removes the definition and its run
/// history.
pub(in crate::gateway) async fn delete_process_handler(
    State(state): State<RouterState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    match store.delete(&id) {
        Ok(true) => {
            notify_trigger_registrar(&state);
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        Ok(false) => not_found(),
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `GET /v1/processes/:id/runs` — run history, newest first, capped at
/// [`aura_store_db::processes::MAX_PROCESS_RUNS_KEPT`].
pub(in crate::gateway) async fn list_process_runs_handler(
    State(state): State<RouterState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(store) = state.process_store.as_ref() else {
        return store_unavailable();
    };
    // Distinguish "unknown process" from "no runs yet".
    match store.get(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return process_error_response(&e).into_response(),
    }
    match store.list_runs(&id) {
        Ok(runs) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "runs": runs })),
        )
            .into_response(),
        Err(e) => process_error_response(&e).into_response(),
    }
}

/// `POST /v1/processes/:id/trigger` — execute the process now.
///
/// Reuses the `POST /v1/run` chat internals: prepares a session, spawns
/// the registered driver task under the process-run id, and enqueues
/// the process prompt as the run's first user message. Runs are
/// asynchronous, so this returns `202 Accepted` with the freshly
/// created run record; poll `GET /v1/processes/:id/runs` (or attach to
/// `WS /stream/:run_id`) for the outcome. `last_run_at` / `next_run_at`
/// are updated by [`ProcessStore::start_run`] before the response.
///
/// Manual triggers fire regardless of the `enabled` flag — the flag
/// gates *scheduled* firing, not explicit operator intent.
pub(in crate::gateway) async fn trigger_process_handler(
    State(state): State<RouterState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let Some(store) = state.process_store.clone() else {
        return store_unavailable();
    };
    let process = match store.get(&id) {
        Ok(Some(p)) => p,
        Ok(None) => return not_found(),
        Err(e) => return process_error_response(&e).into_response(),
    };
    let run = match store.start_run(&id) {
        Ok(run) => run,
        Err(e) => return process_error_response(&e).into_response(),
    };
    // `start_run` advanced last_run_at / next_run_at — re-register the
    // updated schedule with the swarm gateway (best-effort).
    notify_trigger_registrar(&state);

    // Synthetic chat request — the same shape POST /v1/run (Chat)
    // consumes. Identity / workspace use node defaults; the process
    // prompt is delivered as the first user message below. The model
    // must be non-empty for turn dispatch, so resolve it from the
    // process config (`"model"`), then the node's env default.
    let model_id = process
        .config
        .as_ref()
        .and_then(|c| c.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(default_model_id);
    let request = RuntimeRequest {
        r#type: RuntimeRequestType::Chat {
            conversation_messages: Vec::new(),
        },
        agent_identity: Default::default(),
        model: aura_protocol::ModelSelection {
            id: Some(model_id),
            ..Default::default()
        },
        workspace: Default::default(),
        project: None,
        agent_permissions: Default::default(),
        tool_permissions: None,
        agent_capabilities: Default::default(),
        auth_jwt: None,
        user_id: format!("process:{id}"),
    };

    let ctx = WsContext::from_state(&state, None);
    let session = match prepare_chat_session(request, &ctx).await {
        Ok(session) => session,
        Err(e) => {
            let _ = store.finish_run(
                &id,
                &run.run_id,
                ProcessRunStatus::Failure,
                None,
                Some(format!("failed to prepare run session: {}", e.message)),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "ok": false, "error": e.message, "code": e.code })),
            )
                .into_response();
        }
    };

    let handle = spawn_chat_run(session, ctx, run.run_id.clone(), state.chat_runs.clone());

    if handle
        .commands
        .send(InboundMessage::UserMessage(UserMessage {
            content: process.prompt.clone(),
            tool_hints: None,
            attachments: None,
        }))
        .await
        .is_err()
    {
        handle.shutdown.cancel();
        let _ = store.finish_run(
            &id,
            &run.run_id,
            ProcessRunStatus::Failure,
            None,
            Some("run driver rejected the process prompt".into()),
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": "failed to dispatch process prompt" })),
        )
            .into_response();
    }

    tokio::spawn(watch_process_run(
        store,
        handle,
        id.clone(),
        run.run_id.clone(),
    ));

    let stream_url = format!("/stream/{}", run.run_id);
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "ok": true, "run": run, "event_stream_url": stream_url })),
    )
        .into_response()
}

/// Node-default model for triggered runs: same env resolution the
/// provider factory uses (`AURA_DEFAULT_MODEL` / `AURA_ANTHROPIC_MODEL`,
/// then the reasoner's fallback constant).
fn default_model_id() -> String {
    std::env::var("AURA_DEFAULT_MODEL")
        .or_else(|_| std::env::var("AURA_ANTHROPIC_MODEL"))
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| aura_model_reasoner::ENV_FALLBACK_MODEL.to_string())
}

/// Terminal outcome of a triggered run.
enum RunOutcome {
    /// Turn finished cleanly; carries the accumulated text summary.
    Success(Option<String>),
    /// Run surfaced a non-recoverable error (or never completed).
    Failure(String),
}

/// Terminal classification of one outbound frame.
enum TerminalFrame {
    Success,
    Failure(String),
}

/// Inspect one outbound frame: accumulate text into the summary buffer
/// and detect terminal frames.
fn inspect_frame(msg: &OutboundMessage, summary: &mut String) -> Option<TerminalFrame> {
    match msg {
        OutboundMessage::TextDelta(delta) => {
            let remaining = RUN_SUMMARY_MAX_CHARS.saturating_sub(summary.chars().count());
            if remaining > 0 {
                summary.extend(delta.text.chars().take(remaining));
            }
            None
        }
        OutboundMessage::AssistantMessageEnd(_) => Some(TerminalFrame::Success),
        OutboundMessage::Error(e) if !e.recoverable => {
            Some(TerminalFrame::Failure(format!("{}: {}", e.code, e.message)))
        }
        _ => None,
    }
}

/// Watch a triggered run's event channel and persist the terminal
/// status onto its [`ProcessRunRecord`], then tear the one-shot run
/// down (a process run has no interactive client to keep it alive).
async fn watch_process_run(
    store: Arc<ProcessStore>,
    handle: Arc<ChatRunHandle>,
    process_id: String,
    run_id: String,
) {
    let outcome = tokio::time::timeout(TRIGGER_RUN_TIMEOUT, run_outcome(&handle))
        .await
        .unwrap_or_else(|_| {
            RunOutcome::Failure(format!(
                "process run timed out after {}s",
                TRIGGER_RUN_TIMEOUT.as_secs()
            ))
        });

    let result: Result<ProcessRunRecord, ProcessError> = match outcome {
        RunOutcome::Success(summary) => store.finish_run(
            &process_id,
            &run_id,
            ProcessRunStatus::Success,
            summary,
            None,
        ),
        RunOutcome::Failure(error) => store.finish_run(
            &process_id,
            &run_id,
            ProcessRunStatus::Failure,
            None,
            Some(error),
        ),
    };
    if let Err(e) = result {
        tracing::warn!(%process_id, %run_id, error = %e, "failed to persist process run outcome");
    }

    // One-shot semantics: stop the chat driver so the run doesn't
    // linger in the registry waiting for an interactive client.
    handle.shutdown.cancel();
}

/// Wait for the run's terminal frame, assembling a success summary
/// from its text output along the way.
async fn run_outcome(handle: &Arc<ChatRunHandle>) -> RunOutcome {
    let mut summary = String::new();
    let sub = handle.events.subscribe();
    for msg in &sub.history {
        if let Some(terminal) = inspect_frame(msg, &mut summary) {
            return finalize(terminal, summary);
        }
    }
    if sub.already_done {
        return RunOutcome::Failure("run ended before completing a turn".into());
    }
    let mut live = sub.live;
    loop {
        match live.recv().await {
            Ok(msg) => {
                if let Some(terminal) = inspect_frame(&msg, &mut summary) {
                    return finalize(terminal, summary);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return RunOutcome::Failure("run ended before completing a turn".into());
            }
        }
    }
}

/// Attach the accumulated text summary to a success outcome.
fn finalize(terminal: TerminalFrame, summary: String) -> RunOutcome {
    match terminal {
        TerminalFrame::Success => {
            let trimmed = summary.trim();
            RunOutcome::Success((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        TerminalFrame::Failure(error) => RunOutcome::Failure(error),
    }
}
