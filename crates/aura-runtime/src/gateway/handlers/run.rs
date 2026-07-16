//! `POST /v1/run` + `/v1/run/:id/{status,pause,stop}` + `/v1/run/list`
//! handlers — the canonical entry point for chat / dev-loop / task-run
//! kickoffs.
//!
//! Phase A note: this replaces the old `POST /automaton/start` +
//! `AutomatonStartRequest` shape with a single handler that consumes
//! [`aura_protocol::RuntimeRequest`] and dispatches to the right
//! engine surface based on the run kind. Chat runs are prepared
//! synchronously and handed to a driver task registered in
//! [`super::RouterState::chat_runs`] (Part C), so the follow-up
//! `WS /stream/:run_id` attaches non-destructively (history replay +
//! live) and survives socket drops. DevLoop / TaskRun runs are handed
//! to the automaton bridge and exposed via the same `/stream/:run_id`
//! route in event-only mode.

use super::super::*;
use crate::gateway::session::{prepare_chat_session, start_council_run, ChatRequestError};
use aura_protocol::{AgentPermissionsWire, RuntimeRequest, RuntimeRequestType, RuntimeRunResponse};
use uuid::Uuid;

/// `POST /v1/run` — start a chat, dev-loop, or task-run.
///
/// Returns `{ run_id, event_stream_url }`. The caller follows up with
/// `WS /stream/:run_id` to either drive a chat session bidirectionally
/// or receive event-only stream for the automaton runs.
pub(in crate::gateway) async fn run_start_handler(
    headers: HeaderMap,
    State(state): State<RouterState>,
    Json(req): Json<RuntimeRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let auth_token =
        resolve_run_auth_token(req.auth_jwt.clone(), &headers, state.config.require_auth);

    match req.r#type {
        RuntimeRequestType::Chat { .. } => start_chat_run(state, req, auth_token).await,
        RuntimeRequestType::DevLoop {} => start_dev_loop_run(state, req, auth_token).await,
        RuntimeRequestType::TaskRun { .. } => start_task_run(state, req, auth_token).await,
        RuntimeRequestType::Council { .. } => {
            start_council_run_handler(state, req, auth_token).await
        }
    }
}

fn resolve_run_auth_token(
    body_auth_jwt: Option<String>,
    headers: &HeaderMap,
    require_auth: bool,
) -> Option<String> {
    body_auth_jwt.or_else(|| {
        if require_auth {
            return None;
        }
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(String::from)
    })
}

async fn start_chat_run(
    state: RouterState,
    mut req: RuntimeRequest,
    auth_token: Option<String>,
) -> Result<(StatusCode, Json<RuntimeRunResponse>), (StatusCode, Json<serde_json::Value>)> {
    // Make sure the chat-session helpers see the resolved JWT, not
    // whatever the body originally carried (the header version takes
    // precedence per the conventional axum auth flow).
    if auth_token.is_some() {
        req.auth_jwt = auth_token.clone();
    }
    let ctx = crate::gateway::session::WsContext::from_state(&state, auth_token);
    let session =
        prepare_chat_session(req, &ctx)
            .await
            .map_err(|ChatRequestError { code, message }| {
                let status = match code {
                    "invalid_workspace"
                    | "invalid_provider_config"
                    | "tool_permissions_load_failed" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (
                    status,
                    Json(serde_json::json!({"error": message, "code": code})),
                )
            })?;

    let run_id = Uuid::new_v4().to_string();
    // Part C: spawn a driver task that owns the session and turn
    // execution, registered under `run_id`. The follow-up
    // `WS /stream/:run_id` attaches non-destructively (history replay +
    // live) and a dropped socket can reattach without killing the turn.
    crate::gateway::session::spawn_chat_run(session, ctx, run_id.clone(), state.chat_runs.clone());

    let event_stream_url = format!("/stream/{run_id}");
    Ok((
        StatusCode::CREATED,
        Json(RuntimeRunResponse {
            run_id,
            event_stream_url,
        }),
    ))
}

async fn start_council_run_handler(
    state: RouterState,
    mut req: RuntimeRequest,
    auth_token: Option<String>,
) -> Result<(StatusCode, Json<RuntimeRunResponse>), (StatusCode, Json<serde_json::Value>)> {
    // Header-resolved JWT takes precedence (same convention as the chat
    // path), so the council session + member dispatch see the right token.
    if auth_token.is_some() {
        req.auth_jwt = auth_token.clone();
    }
    let ctx = crate::gateway::session::WsContext::from_state(&state, auth_token);
    let run_id =
        start_council_run(req, ctx)
            .await
            .map_err(|ChatRequestError { code, message }| {
                let status = match code {
                    "council_no_members"
                    | "invalid_council_request"
                    | "invalid_second_opinion_request"
                    | "second_opinion_no_references"
                    | "invalid_workspace"
                    | "invalid_provider_config"
                    | "tool_permissions_load_failed" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (
                    status,
                    Json(serde_json::json!({"error": message, "code": code})),
                )
            })?;

    let event_stream_url = format!("/stream/{run_id}");
    Ok((
        StatusCode::CREATED,
        Json(RuntimeRunResponse {
            run_id,
            event_stream_url,
        }),
    ))
}

async fn start_dev_loop_run(
    state: RouterState,
    req: RuntimeRequest,
    auth_token: Option<String>,
) -> Result<(StatusCode, Json<RuntimeRunResponse>), (StatusCode, Json<serde_json::Value>)> {
    let bridge = automaton_bridge(&state)?;

    let RuntimeRequest {
        r#type: _,
        agent_identity,
        model,
        workspace,
        project,
        agent_permissions,
        tool_permissions: _,
        agent_capabilities,
        auth_jwt: _,
        user_id: _,
    } = req;

    let project_ctx = project.ok_or_else(|| {
        bad_request("dev-loop runs require a project context (project_id, billing ids)")
    })?;
    let workspace_root = workspace.project_path.map(|s| {
        let path = std::path::PathBuf::from(s);
        state.config.resolve_project_path(&path)
    });
    let agent_permissions = wire_permissions_to_core(agent_permissions);
    let agent_persona = agent_identity.persona.filter(|p| !p.is_empty());
    let agent_skills = agent_identity.skills;
    let agent_system_prompt = agent_identity
        .system_prompt
        .filter(|s| !s.trim().is_empty());

    let installed_tools = if agent_capabilities.installed_tools.is_empty() {
        None
    } else {
        Some(agent_capabilities.installed_tools)
    };
    let installed_integrations = if agent_capabilities.installed_integrations.is_empty() {
        None
    } else {
        Some(agent_capabilities.installed_integrations)
    };

    let automaton_id = bridge
        .start_dev_loop_with_capabilities(
            &project_ctx.project_id,
            workspace_root,
            auth_token,
            model.id,
            workspace.git_repo_url,
            workspace.git_branch,
            installed_tools,
            installed_integrations,
            agent_permissions,
            project_ctx.aura_org_id,
            project_ctx.aura_session_id,
            project_ctx.aura_agent_id,
            agent_persona,
            agent_skills,
            agent_system_prompt,
        )
        .await
        .map_err(run_start_error_response)?;

    let event_stream_url = format!("/stream/{automaton_id}");
    Ok((
        StatusCode::CREATED,
        Json(RuntimeRunResponse {
            run_id: automaton_id,
            event_stream_url,
        }),
    ))
}

async fn start_task_run(
    state: RouterState,
    req: RuntimeRequest,
    auth_token: Option<String>,
) -> Result<(StatusCode, Json<RuntimeRunResponse>), (StatusCode, Json<serde_json::Value>)> {
    let bridge = automaton_bridge(&state)?;

    let RuntimeRequest {
        r#type,
        agent_identity,
        model,
        workspace,
        project,
        agent_permissions,
        tool_permissions: _,
        agent_capabilities,
        auth_jwt: _,
        user_id: _,
    } = req;

    let (task_id, prior_failure, work_log) = match r#type {
        RuntimeRequestType::TaskRun {
            task_id,
            prior_failure,
            work_log,
        } => (task_id, prior_failure, work_log),
        _ => unreachable!("dispatched to task_run for non-TaskRun variant"),
    };

    let project_ctx = project.ok_or_else(|| {
        bad_request("task-run runs require a project context (project_id, billing ids)")
    })?;
    let workspace_root = workspace.project_path.map(|s| {
        let path = std::path::PathBuf::from(s);
        state.config.resolve_project_path(&path)
    });
    let agent_permissions = wire_permissions_to_core(agent_permissions);
    let agent_persona = agent_identity.persona.filter(|p| !p.is_empty());
    let agent_skills = agent_identity.skills;
    let agent_system_prompt = agent_identity
        .system_prompt
        .filter(|s| !s.trim().is_empty());

    let installed_tools = if agent_capabilities.installed_tools.is_empty() {
        None
    } else {
        Some(agent_capabilities.installed_tools)
    };
    let installed_integrations = if agent_capabilities.installed_integrations.is_empty() {
        None
    } else {
        Some(agent_capabilities.installed_integrations)
    };

    let automaton_id = bridge
        .run_task_with_capabilities(
            &project_ctx.project_id,
            &task_id,
            workspace_root,
            auth_token,
            model.id,
            workspace.git_repo_url,
            workspace.git_branch,
            installed_tools,
            installed_integrations,
            agent_permissions,
            prior_failure,
            work_log,
            project_ctx.aura_org_id,
            project_ctx.aura_session_id,
            project_ctx.aura_agent_id,
            agent_persona,
            agent_skills,
            agent_system_prompt,
        )
        .await
        .map_err(run_start_error_response)?;

    let event_stream_url = format!("/stream/{automaton_id}");
    Ok((
        StatusCode::CREATED,
        Json(RuntimeRunResponse {
            run_id: automaton_id,
            event_stream_url,
        }),
    ))
}

fn automaton_bridge(
    state: &RouterState,
) -> Result<
    std::sync::Arc<aura_engine::automaton::AutomatonBridge>,
    (StatusCode, Json<serde_json::Value>),
> {
    state.automaton_bridge.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "automaton controller unavailable"})),
        )
    })
}

fn wire_permissions_to_core(wire: AgentPermissionsWire) -> aura_core_types::AgentPermissions {
    crate::gateway::session::agent_permissions_from_wire(wire)
}

fn bad_request(message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": message})),
    )
}

#[cfg(test)]
mod tests {
    use super::resolve_run_auth_token;
    use axum::http::{header, HeaderMap, HeaderValue};

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn run_auth_prefers_body_jwt() {
        let headers = headers_with_bearer("transport-or-legacy-jwt");
        assert_eq!(
            resolve_run_auth_token(Some("user-jwt".to_string()), &headers, true).as_deref(),
            Some("user-jwt")
        );
    }

    #[test]
    fn run_auth_ignores_transport_bearer_when_router_auth_is_required() {
        let headers = headers_with_bearer("node-transport-secret");
        assert_eq!(resolve_run_auth_token(None, &headers, true), None);
    }

    #[test]
    fn run_auth_accepts_bearer_fallback_when_router_auth_is_disabled() {
        let headers = headers_with_bearer("legacy-user-jwt");
        assert_eq!(
            resolve_run_auth_token(None, &headers, false).as_deref(),
            Some("legacy-user-jwt")
        );
    }
}

fn run_start_error_response(e: String) -> (StatusCode, Json<serde_json::Value>) {
    let lowered = e.to_ascii_lowercase();
    let status = if lowered.contains("already running") {
        StatusCode::CONFLICT
    } else if lowered.starts_with("missing model") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(serde_json::json!({"error": e})))
}

/// `GET /v1/run/:run_id/status` — fetch the status of a running run.
///
/// Live chat runs (a registered driver task) return
/// `{"status": "chat_active"}`. A child subagent run is registered in
/// the same registry, so it is looked up here identically and reports
/// `kind: "subagent"` plus its parent-linkage metadata. Automaton runs
/// delegate to the existing bridge status.
pub(in crate::gateway) async fn run_status_handler(
    State(state): State<RouterState>,
    Path(run_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if let Some(handle) = state.chat_runs.get(&run_id).map(|e| e.value().clone()) {
        let mut body =
            serde_json::json!({"run_id": run_id, "status": "chat_active", "kind": "chat"});
        if let Some(linkage) = &handle.linkage {
            body["kind"] = serde_json::json!("subagent");
            body["parent_run_id"] = serde_json::json!(linkage.parent_run_id);
            body["parent_tool_use_id"] = serde_json::json!(linkage.parent_tool_use_id);
            body["child_run_id"] = serde_json::json!(linkage.child_run_id);
            body["depth"] = serde_json::json!(linkage.depth);
            body["parent_chain"] = serde_json::json!(linkage.parent_chain);
        }
        return Ok(Json(body));
    }
    let bridge = automaton_bridge(&state)?;
    match bridge.get_status(&run_id) {
        Some(info) => Ok(Json(serde_json::to_value(&info).unwrap_or_default())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("run {run_id} not found")})),
        )),
    }
}

/// `GET /v1/run/list` — list every active automaton run on this node.
///
/// Chat runs aren't enumerated today because they're per-WebSocket and
/// hold no server-side identity beyond the pending-attach map entry.
pub(in crate::gateway) async fn run_list_handler(
    State(state): State<RouterState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let bridge = automaton_bridge(&state)?;
    let list = bridge.list_automatons();
    Ok(Json(
        serde_json::to_value(&list).unwrap_or(serde_json::json!([])),
    ))
}

/// `POST /v1/run/:run_id/pause` — pause an active automaton run.
pub(in crate::gateway) async fn run_pause_handler(
    State(state): State<RouterState>,
    Path(run_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let bridge = automaton_bridge(&state)?;
    bridge
        .pause_by_id(&run_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))))?;
    Ok(Json(
        serde_json::json!({"ok": true, "run_id": run_id, "status": "paused"}),
    ))
}

/// `POST /v1/run/:run_id/stop` — stop a run.
///
/// For chat runs, removes the registry entry and signals the driver's
/// `shutdown` token so the run tears down (cancelling any active turn).
/// Automaton runs delegate to the bridge.
pub(in crate::gateway) async fn run_stop_handler(
    State(state): State<RouterState>,
    Path(run_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if let Some((_, handle)) = state.chat_runs.remove(&run_id) {
        // Signal the driver to tear down (cancelling any active turn);
        // the entry is already removed so no late attach finds it.
        handle.shutdown.cancel();
        return Ok(Json(
            serde_json::json!({"ok": true, "run_id": run_id, "status": "stopped"}),
        ));
    }
    let bridge = automaton_bridge(&state)?;
    bridge
        .stop_by_id(&run_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))))?;
    Ok(Json(
        serde_json::json!({"ok": true, "run_id": run_id, "status": "stopped"}),
    ))
}
