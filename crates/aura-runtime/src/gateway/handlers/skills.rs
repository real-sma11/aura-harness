//! Skills CRUD API endpoints — list, get, create, activate, and per-agent install/uninstall.

use super::super::RouterState;
use super::util::parse_agent_id;
use aura_context_skills::{
    SkillActivation, SkillFrontmatter, SkillInstallation, SkillManager, SkillMeta, SkillSource,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn skill_err(e: aura_context_skills::SkillError) -> (StatusCode, Json<serde_json::Value>) {
    let status = if e.is_not_found() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}

fn require_skills(
    state: &RouterState,
) -> Result<&Arc<RwLock<SkillManager>>, (StatusCode, Json<serde_json::Value>)> {
    state.skill_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "skill system not configured" })),
        )
    })
}

/// Response for a single skill (frontmatter + body).
#[derive(Serialize)]
pub(in crate::gateway) struct SkillDetail {
    name: String,
    description: String,
    source: SkillSource,
    body: String,
    frontmatter: SkillFrontmatter,
}

/// Response for skill activation.
#[derive(Serialize)]
pub(in crate::gateway) struct ActivationResponse {
    skill_name: String,
    rendered_content: String,
    allowed_tools: Vec<String>,
    fork_context: bool,
    agent_type: Option<String>,
}

impl From<SkillActivation> for ActivationResponse {
    fn from(a: SkillActivation) -> Self {
        Self {
            skill_name: a.skill_name,
            rendered_content: a.rendered_content,
            allowed_tools: a.allowed_tools,
            fork_context: a.fork_context,
            agent_type: a.agent_type,
        }
    }
}

/// `GET /api/skills` — list all skills (metadata only).
pub(in crate::gateway) async fn list_skills(
    State(state): State<RouterState>,
) -> ApiResult<Vec<SkillMeta>> {
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    Ok(Json(guard.list_all()))
}

/// `GET /api/skills/:name` — get full skill details.
pub(in crate::gateway) async fn get_skill(
    State(state): State<RouterState>,
    Path(name): Path<String>,
) -> ApiResult<SkillDetail> {
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    let skill = guard.get(&name).map_err(skill_err)?;
    Ok(Json(SkillDetail {
        name: skill.frontmatter.name.clone(),
        description: skill.frontmatter.description.clone(),
        source: skill.source.clone(),
        body: skill.body.clone(),
        frontmatter: skill.frontmatter.clone(),
    }))
}

/// Request body for skill activation.
#[derive(Deserialize)]
pub(in crate::gateway) struct ActivateBody {
    #[serde(default)]
    pub arguments: String,
}

/// `POST /api/skills/:name/activate` — activate a skill with arguments.
pub(in crate::gateway) async fn activate_skill(
    State(state): State<RouterState>,
    Path(name): Path<String>,
    Json(body): Json<ActivateBody>,
) -> ApiResult<ActivationResponse> {
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    let activation = guard.activate(&name, &body.arguments).map_err(skill_err)?;
    Ok(Json(activation.into()))
}

// -- Skill creation --

/// Request body for creating a new skill.
#[derive(Deserialize)]
pub(in crate::gateway) struct CreateSkillBody {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub user_invocable: bool,
    #[serde(default)]
    pub agent_target: Option<SkillAgentTargetBody>,
}

#[derive(Deserialize)]
pub(in crate::gateway) struct SkillAgentTargetBody {
    pub agent_id: String,
    pub name: String,
}

/// `POST /api/skills` — create a new skill (writes SKILL.md to personal dir).
pub(in crate::gateway) async fn create_skill(
    State(state): State<RouterState>,
    Json(body): Json<CreateSkillBody>,
) -> Result<(StatusCode, Json<SkillDetail>), (StatusCode, Json<serde_json::Value>)> {
    let mgr = require_skills(&state)?;
    let mut guard = mgr.write().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    let target_id = body
        .agent_target
        .as_ref()
        .map(|target| target.agent_id.as_str());
    let target_name = body
        .agent_target
        .as_ref()
        .map(|target| target.name.as_str());
    let skill = guard
        .create_with_agent_target(
            &body.name,
            &body.description,
            &body.body,
            body.user_invocable,
            target_id,
            target_name,
        )
        .map_err(skill_err)?;
    Ok((
        StatusCode::CREATED,
        Json(SkillDetail {
            name: skill.frontmatter.name.clone(),
            description: skill.frontmatter.description.clone(),
            source: skill.source.clone(),
            body: skill.body.clone(),
            frontmatter: skill.frontmatter.clone(),
        }),
    ))
}

// -- Per-agent installation endpoints --

/// Request body for installing a skill for an agent.
#[derive(Deserialize)]
pub(in crate::gateway) struct InstallBody {
    pub name: String,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub approved_paths: Vec<String>,
    #[serde(default)]
    pub approved_commands: Vec<String>,
}

/// `GET /api/agents/:agent_id/skills` — list skills installed for an agent.
pub(in crate::gateway) async fn list_agent_skills(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<Vec<SkillInstallation>> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    let installations = guard.list_agent_skills(agent_id).map_err(skill_err)?;
    Ok(Json(installations))
}

/// `POST /api/agents/:agent_id/skills` — install a skill for an agent.
pub(in crate::gateway) async fn install_agent_skill(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(body): Json<InstallBody>,
) -> Result<(StatusCode, Json<SkillInstallation>), (StatusCode, Json<serde_json::Value>)> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    let installation = guard
        .install_for_agent(
            agent_id,
            &body.name,
            body.source_url,
            body.approved_paths,
            body.approved_commands,
        )
        .map_err(skill_err)?;
    Ok((StatusCode::CREATED, Json(installation)))
}

/// `DELETE /api/agents/:agent_id/skills/:name` — uninstall a skill from an agent.
pub(in crate::gateway) async fn uninstall_agent_skill(
    State(state): State<RouterState>,
    Path((agent_hex, name)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let mgr = require_skills(&state)?;
    let guard = mgr.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "lock poisoned" })),
        )
    })?;
    guard
        .uninstall_from_agent(agent_id, &name)
        .map_err(skill_err)?;
    Ok(StatusCode::NO_CONTENT)
}
