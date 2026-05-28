use super::super::errors::ApiError;
use super::super::*;
use super::util::parse_agent_id;
use crate::tool_permissions::{
    append_agent_tool_permissions_entry, effective_tool_infos, enforce_monotonic_update,
    load_agent_tool_context, validate_agent_tool_permissions, validate_user_defaults,
    EffectiveToolInfo,
};
use aura_core::{AgentToolPermissions, UserToolDefaults};

#[derive(Debug, Deserialize)]
pub(in crate::gateway) struct AgentToolsQuery {
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(in crate::gateway) struct AgentToolPermissionsResponse {
    tool_permissions: Option<AgentToolPermissions>,
}

#[derive(Debug, Serialize)]
pub(in crate::gateway) struct AgentToolsResponse {
    tools: Vec<EffectiveToolInfo>,
}

pub(in crate::gateway) async fn get_user_tool_defaults_handler(
    State(state): State<RouterState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let defaults = state
        .store
        .get_user_tool_defaults(&user_id)
        .map_err(storage_error)?
        .unwrap_or_default();
    Ok(Json(defaults))
}

pub(in crate::gateway) async fn put_user_tool_defaults_handler(
    State(state): State<RouterState>,
    Path(user_id): Path<String>,
    Json(defaults): Json<UserToolDefaults>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    validate_user_defaults(&defaults, &state.catalog).map_err(bad_request)?;
    state
        .store
        .put_user_tool_defaults(&user_id, &defaults)
        .map_err(storage_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn get_agent_tool_permissions_handler(
    State(state): State<RouterState>,
    Path(agent_id_hex): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let agent_id = parse_agent_id(&agent_id_hex).map_err(ApiError::into_string_tuple)?;
    let context =
        load_agent_tool_context(state.store.as_ref(), agent_id).map_err(storage_string_error)?;
    Ok(Json(AgentToolPermissionsResponse {
        tool_permissions: context.tool_permissions,
    }))
}

pub(in crate::gateway) async fn put_agent_tool_permissions_handler(
    State(state): State<RouterState>,
    Path(agent_id_hex): Path<String>,
    Json(next): Json<AgentToolPermissions>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let agent_id = parse_agent_id(&agent_id_hex).map_err(ApiError::into_string_tuple)?;
    validate_agent_tool_permissions(&next, &state.catalog).map_err(bad_request)?;

    let context =
        load_agent_tool_context(state.store.as_ref(), agent_id).map_err(storage_string_error)?;
    let user_default = load_user_default_for_agent(&state, context.originating_user_id.as_ref())?;
    enforce_monotonic_update(&user_default, context.tool_permissions.as_ref(), &next)
        .map_err(|e| (StatusCode::FORBIDDEN, e))?;

    let tx = append_agent_tool_permissions_entry(&state.store, &state.scheduler, agent_id, &next)
        .await
        .map_err(storage_string_error)?;
    let scheduler = state.scheduler.clone();
    tokio::spawn(async move {
        if let Err(e) = scheduler.schedule_agent(agent_id).await {
            warn!(
                agent_id = %agent_id,
                error = %e,
                "failed to schedule agent after tool permission update"
            );
        }
    });
    info!(
        agent_id = %agent_id,
        tx_hash = %tx.hash,
        "Updated agent tool permissions; active sessions should refresh policy"
    );

    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn get_agent_tools_handler(
    State(state): State<RouterState>,
    Path(agent_id_hex): Path<String>,
    Query(query): Query<AgentToolsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let agent_id = parse_agent_id(&agent_id_hex).map_err(ApiError::into_string_tuple)?;
    let context =
        load_agent_tool_context(state.store.as_ref(), agent_id).map_err(storage_string_error)?;
    let user_id = query.user_id.or(context.originating_user_id);
    let user_default = load_user_default_for_agent(&state, user_id.as_ref())?;
    let tools = effective_tool_infos(
        &state.catalog,
        &state.tool_config,
        &user_default,
        context.tool_permissions.as_ref(),
        Some(&context.agent_permissions),
    );
    Ok(Json(AgentToolsResponse { tools }))
}

fn load_user_default_for_agent(
    state: &RouterState,
    user_id: Option<&String>,
) -> Result<UserToolDefaults, (StatusCode, String)> {
    let Some(user_id) = user_id.map(String::as_str) else {
        return Ok(UserToolDefaults::default());
    };
    state
        .store
        .get_user_tool_defaults(user_id)
        .map_err(storage_error)
        .map(|defaults| defaults.unwrap_or_default())
}

fn bad_request(message: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message)
}

fn storage_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Storage error: {error}"),
    )
}

fn storage_string_error(message: String) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}
