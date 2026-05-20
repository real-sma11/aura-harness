//! Domain API trait and lightweight descriptor types.
//!
//! `DomainApi` is the callback seam that allows the harness tool layer to
//! invoke application-level domain operations (specs, tasks, projects, etc.)
//! without depending on the concrete app crate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Descriptor types – lightweight DTOs that avoid pulling in app domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecDescriptor {
    #[serde(alias = "spec_id")]
    pub id: String,
    #[serde(
        alias = "projectId",
        default,
        deserialize_with = "super::helpers::deser_string_or_default"
    )]
    pub project_id: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub title: String,
    #[serde(
        alias = "markdownContents",
        alias = "markdown_contents",
        default,
        deserialize_with = "super::helpers::deser_string_or_default"
    )]
    pub content: String,
    #[serde(
        alias = "orderIndex",
        alias = "order_index",
        default,
        deserialize_with = "super::helpers::deser_u32_or_default"
    )]
    pub order: u32,
    #[serde(alias = "parentId", default)]
    pub parent_id: Option<String>,
    /// Optimistic-concurrency token (blake3 hex of `markdown_contents`)
    /// returned by aura-os-server. Surfaced to the LLM by `get_spec` and
    /// passed back as `if_match` on `update_spec` / `update_spec_section` /
    /// `append_to_spec` so a stale edit is refused. `None` when the
    /// backend does not advertise a hash.
    #[serde(alias = "contentHash", alias = "content_hash", default)]
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDescriptor {
    #[serde(alias = "task_id", alias = "taskId")]
    pub id: String,
    #[serde(
        alias = "specId",
        default,
        deserialize_with = "super::helpers::deser_string_or_default"
    )]
    pub spec_id: String,
    #[serde(
        alias = "projectId",
        default,
        deserialize_with = "super::helpers::deser_string_or_default"
    )]
    pub project_id: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub title: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub description: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub status: String,
    #[serde(alias = "dependencyIds", alias = "dependency_ids", default)]
    pub dependencies: Vec<String>,
    #[serde(
        alias = "orderIndex",
        alias = "order_index",
        default,
        deserialize_with = "super::helpers::deser_u32_or_default"
    )]
    pub order: u32,
}

#[cfg(test)]
mod tests {
    use super::TaskDescriptor;

    #[test]
    fn task_descriptor_accepts_aura_os_task_shape() {
        let task: TaskDescriptor = serde_json::from_value(serde_json::json!({
            "task_id": "6b502ffa-9da7-4631-9090-388415fa8ddb",
            "project_id": "2a7f56ff-48c5-4e58-90c0-f62a69084568",
            "spec_id": "69a95f6f-28c6-4ce8-9cf9-e1c7cc6dff0a",
            "title": "Wire up live output",
            "description": "Make automation frames appear in the UI",
            "status": "pending",
            "order_index": 3,
            "dependency_ids": [
                "dc0fb195-9a6c-4b33-8514-f15e011285f8"
            ]
        }))
        .expect("aura-os Task should deserialize as TaskDescriptor");

        assert_eq!(task.id, "6b502ffa-9da7-4631-9090-388415fa8ddb");
        assert_eq!(task.project_id, "2a7f56ff-48c5-4e58-90c0-f62a69084568");
        assert_eq!(task.spec_id, "69a95f6f-28c6-4ce8-9cf9-e1c7cc6dff0a");
        assert_eq!(task.status, "pending");
        assert_eq!(task.order, 3);
        assert_eq!(
            task.dependencies,
            vec!["dc0fb195-9a6c-4b33-8514-f15e011285f8".to_string()]
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDescriptor {
    #[serde(alias = "project_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(alias = "linked_folder_path", default)]
    pub path: String,
    pub description: Option<String>,
    pub tech_stack: Option<String>,
    pub build_command: Option<String>,
    pub test_command: Option<String>,
}

/// A project-bound agent instance returned by `list_project_agents` and
/// `create_project_agent`. Mirrors aura-os's `AgentInstance` shape but only
/// captures the fields the assign-agent flow needs to detect duplicates and
/// hand back a usable handle to the caller.
///
/// Serializes / deserializes the instance id as `agent_instance_id` to match
/// the canonical aura-os API contract (the marketplace `Hire` endpoint and
/// `GET /api/projects/{id}/agents` both use this name). The `id` alias keeps
/// us forward-compatible with any future server payload that drops the
/// `agent_instance_` prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInstanceDescriptor {
    #[serde(rename = "agent_instance_id", alias = "id")]
    pub id: String,
    /// Template agent id this instance was minted from. Used for duplicate
    /// detection in `assign_agent_to_project`.
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub agent_id: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub project_id: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub name: String,
    #[serde(default, deserialize_with = "super::helpers::deser_string_or_default")]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDescriptor {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDescriptor {
    pub id: String,
    pub instance_id: String,
    pub project_id: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Update / param types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub order_index: Option<u32>,
    pub dependency_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub tech_stack: Option<String>,
    pub build_command: Option<String>,
    pub test_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveMessageParams {
    pub project_id: String,
    pub instance_id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionParams {
    pub instance_id: String,
    pub project_id: String,
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// DomainApi trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait DomainApi: Send + Sync {
    // Specs — JWT auth via /api/ routes
    async fn list_specs(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>>;
    async fn get_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<SpecDescriptor>;
    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor>;
    async fn update_spec(
        &self,
        spec_id: &str,
        title: Option<&str>,
        content: Option<&str>,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor>;
    /// Replace a single `## ` section of a spec without re-sending the
    /// whole body. Defaults to an error so only backends that support
    /// granular edits (the HTTP impl) need to implement it.
    async fn update_spec_section(
        &self,
        spec_id: &str,
        section_heading: &str,
        new_body: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let _ = (spec_id, section_heading, new_body, if_match, jwt);
        Err(anyhow::anyhow!(
            "update_spec_section is not supported by this DomainApi implementation"
        ))
    }
    /// Append a markdown block to a spec without re-sending the body.
    /// Defaults to an error for the same reason as `update_spec_section`.
    async fn append_to_spec(
        &self,
        spec_id: &str,
        markdown: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let _ = (spec_id, markdown, if_match, jwt);
        Err(anyhow::anyhow!(
            "append_to_spec is not supported by this DomainApi implementation"
        ))
    }
    async fn delete_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<()>;

    // Tasks — JWT auth via /api/ routes
    async fn list_tasks(
        &self,
        project_id: &str,
        spec_id: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>>;
    #[allow(clippy::too_many_arguments)]
    async fn create_task(
        &self,
        project_id: &str,
        spec_id: &str,
        title: &str,
        description: &str,
        dependencies: &[String],
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor>;
    async fn update_task(
        &self,
        task_id: &str,
        updates: TaskUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor>;
    async fn delete_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<()>;
    async fn transition_task(
        &self,
        task_id: &str,
        status: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor>;
    async fn claim_next_task(
        &self,
        project_id: &str,
        agent_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>>;

    // Single task lookup — JWT auth via /api/ routes
    async fn get_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<TaskDescriptor>;

    // Project (aura-network) — JWT auth via /api/ routes
    async fn get_project(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor>;
    async fn update_project(
        &self,
        project_id: &str,
        updates: ProjectUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor>;

    // Project-agent instances (aura-os-server) — JWT auth via /api/ routes.
    //
    // Default impls return `unimplemented` so the >5 existing `impl DomainApi`
    // sites (mocks, kernel-domain-gateway, automaton fakes, JWT wrapper) keep
    // compiling. The only path that actually exercises these is
    // `HttpDomainApi`, which overrides both.
    async fn list_project_agents(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<AgentInstanceDescriptor>> {
        Err(anyhow::anyhow!(
            "list_project_agents not implemented for this DomainApi"
        ))
    }
    async fn create_project_agent(
        &self,
        _project_id: &str,
        _agent_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<AgentInstanceDescriptor> {
        Err(anyhow::anyhow!(
            "create_project_agent not implemented for this DomainApi"
        ))
    }

    // Storage: logs — JWT auth via /api/ routes
    async fn create_log(
        &self,
        project_id: &str,
        message: &str,
        level: &str,
        agent_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value>;
    async fn list_logs(
        &self,
        project_id: &str,
        level: Option<&str>,
        limit: Option<u64>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value>;
    async fn get_project_stats(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value>;

    // Messages
    async fn list_messages(
        &self,
        project_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>>;
    async fn save_message(&self, params: SaveMessageParams) -> anyhow::Result<()>;

    // Sessions
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor>;
    async fn get_active_session(
        &self,
        instance_id: &str,
    ) -> anyhow::Result<Option<SessionDescriptor>>;

    // Orbit (raw JSON pass-through)
    async fn orbit_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String>;
    fn orbit_url(&self) -> &str {
        ""
    }

    // Network (raw JSON pass-through)
    async fn network_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String>;
}
