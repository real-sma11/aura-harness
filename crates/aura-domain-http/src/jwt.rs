//! JWT-injecting `DomainApi` wrapper.
//!
//! Automatons call `DomainApi` methods with `jwt: None` because they run as
//! internal services without user context.  This wrapper captures a JWT at
//! construction time and transparently injects it whenever the caller passes
//! `None`.
//!
//! Phase C / Commit 4 relocates this here alongside the HTTP
//! `DomainApi` impl. The automaton bridge in `aura-engine` now imports
//! [`JwtDomainApi`] from this crate.

use std::sync::Arc;

use async_trait::async_trait;
use aura_tools::domain_tools::{
    CreateSessionParams, DomainApi, ListMarketplaceAgentsParams, ListMarketplaceAgentsResponse,
    MessageDescriptor, ProjectDescriptor, ProjectUpdate, SaveMessageParams, SessionDescriptor,
    SpecDescriptor, TaskDescriptor, TaskUpdate,
};

/// Wraps an inner [`DomainApi`] and stamps a captured JWT onto every
/// call site that did not supply one.
pub struct JwtDomainApi {
    inner: Arc<dyn DomainApi>,
    jwt: String,
}

impl JwtDomainApi {
    pub fn new(inner: Arc<dyn DomainApi>, jwt: String) -> Self {
        Self { inner, jwt }
    }

    fn jwt_or<'a>(&'a self, caller: Option<&'a str>) -> Option<&'a str> {
        caller.or(Some(&self.jwt))
    }
}

#[async_trait]
impl DomainApi for JwtDomainApi {
    async fn list_specs(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        self.inner.list_specs(project_id, self.jwt_or(jwt)).await
    }
    async fn get_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        self.inner.get_spec(spec_id, self.jwt_or(jwt)).await
    }
    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.inner
            .create_spec(project_id, title, content, order, self.jwt_or(jwt))
            .await
    }
    async fn update_spec(
        &self,
        spec_id: &str,
        title: Option<&str>,
        content: Option<&str>,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.inner
            .update_spec(spec_id, title, content, if_match, self.jwt_or(jwt))
            .await
    }
    async fn update_spec_section(
        &self,
        spec_id: &str,
        section_heading: &str,
        new_body: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.inner
            .update_spec_section(
                spec_id,
                section_heading,
                new_body,
                if_match,
                self.jwt_or(jwt),
            )
            .await
    }
    async fn append_to_spec(
        &self,
        spec_id: &str,
        markdown: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.inner
            .append_to_spec(spec_id, markdown, if_match, self.jwt_or(jwt))
            .await
    }
    async fn delete_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        self.inner.delete_spec(spec_id, self.jwt_or(jwt)).await
    }
    async fn list_tasks(
        &self,
        project_id: &str,
        spec_id: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        self.inner
            .list_tasks(project_id, spec_id, self.jwt_or(jwt))
            .await
    }
    async fn create_task(
        &self,
        project_id: &str,
        spec_id: &str,
        title: &str,
        description: &str,
        dependencies: &[String],
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.inner
            .create_task(
                project_id,
                spec_id,
                title,
                description,
                dependencies,
                order,
                self.jwt_or(jwt),
            )
            .await
    }
    async fn update_task(
        &self,
        task_id: &str,
        updates: TaskUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.inner
            .update_task(task_id, updates, self.jwt_or(jwt))
            .await
    }
    async fn delete_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        self.inner.delete_task(task_id, self.jwt_or(jwt)).await
    }
    async fn transition_task(
        &self,
        task_id: &str,
        status: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.inner
            .transition_task(task_id, status, self.jwt_or(jwt))
            .await
    }
    async fn claim_next_task(
        &self,
        project_id: &str,
        agent_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        self.inner
            .claim_next_task(project_id, agent_id, self.jwt_or(jwt))
            .await
    }
    async fn get_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        self.inner.get_task(task_id, self.jwt_or(jwt)).await
    }
    async fn get_project(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        self.inner.get_project(project_id, self.jwt_or(jwt)).await
    }
    async fn update_project(
        &self,
        project_id: &str,
        updates: ProjectUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        self.inner
            .update_project(project_id, updates, self.jwt_or(jwt))
            .await
    }
    async fn create_log(
        &self,
        project_id: &str,
        message: &str,
        level: &str,
        agent_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner
            .create_log(
                project_id,
                message,
                level,
                agent_id,
                metadata,
                self.jwt_or(jwt),
            )
            .await
    }
    async fn list_logs(
        &self,
        project_id: &str,
        level: Option<&str>,
        limit: Option<u64>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner
            .list_logs(project_id, level, limit, self.jwt_or(jwt))
            .await
    }
    async fn get_project_stats(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner
            .get_project_stats(project_id, self.jwt_or(jwt))
            .await
    }
    async fn list_messages(
        &self,
        project_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>> {
        self.inner.list_messages(project_id, instance_id).await
    }
    async fn save_message(&self, params: SaveMessageParams) -> anyhow::Result<()> {
        self.inner.save_message(params).await
    }
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor> {
        self.inner.create_session(params).await
    }
    async fn get_active_session(
        &self,
        instance_id: &str,
    ) -> anyhow::Result<Option<SessionDescriptor>> {
        self.inner.get_active_session(instance_id).await
    }
    async fn orbit_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        self.inner
            .orbit_api_call(method, path, body, self.jwt_or(jwt))
            .await
    }
    fn orbit_url(&self) -> &str {
        self.inner.orbit_url()
    }
    async fn network_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        self.inner
            .network_api_call(method, path, body, self.jwt_or(jwt))
            .await
    }
    async fn list_marketplace_agents(
        &self,
        params: ListMarketplaceAgentsParams<'_>,
        jwt: Option<&str>,
    ) -> anyhow::Result<ListMarketplaceAgentsResponse> {
        self.inner
            .list_marketplace_agents(params, self.jwt_or(jwt))
            .await
    }
}
