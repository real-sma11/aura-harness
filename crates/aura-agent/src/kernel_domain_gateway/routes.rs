//! `DomainApi` implementation for [`KernelDomainGateway`].
//!
//! Each method is either a passthrough (read-only verbs and the few
//! `GET`/`HEAD`/`OPTIONS` orbit/network calls) or a `with_recording!`-
//! bracketed call that emits a request/response `RecordEntry` pair via
//! [`KernelDomainGateway::record_request`](super::handle::KernelDomainGateway::record_request) and
//! [`KernelDomainGateway::record_response`](super::handle::KernelDomainGateway::record_response).

use async_trait::async_trait;
use aura_tools::domain_tools::{
    CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
    SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
};
use serde_json::json;

use super::handle::KernelDomainGateway;
use super::wire::{with_recording, with_recording_unit};

#[async_trait]
impl DomainApi for KernelDomainGateway {
    // --- Specs ----------------------------------------------------------
    async fn list_specs(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        self.inner.list_specs(project_id, jwt).await
    }

    async fn get_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        self.inner.get_spec(spec_id, jwt).await
    }

    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        with_recording!(
            self,
            "create_spec",
            json!({
                "project_id": project_id,
                "title": title,
                "order": order,
                "content_bytes": content.len(),
            }),
            self.inner
                .create_spec(project_id, title, content, order, jwt)
        )
    }

    async fn update_spec(
        &self,
        spec_id: &str,
        title: Option<&str>,
        content: Option<&str>,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        with_recording!(
            self,
            "update_spec",
            json!({
                "spec_id": spec_id,
                "title_set": title.is_some(),
                "content_bytes": content.map(str::len),
                "if_match_set": if_match.is_some(),
            }),
            self.inner
                .update_spec(spec_id, title, content, if_match, jwt)
        )
    }

    async fn update_spec_section(
        &self,
        spec_id: &str,
        section_heading: &str,
        new_body: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        with_recording!(
            self,
            "update_spec_section",
            json!({
                "spec_id": spec_id,
                "section_heading": section_heading,
                "new_body_bytes": new_body.len(),
                "if_match_set": if_match.is_some(),
            }),
            self.inner
                .update_spec_section(spec_id, section_heading, new_body, if_match, jwt)
        )
    }

    async fn append_to_spec(
        &self,
        spec_id: &str,
        markdown: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        with_recording!(
            self,
            "append_to_spec",
            json!({
                "spec_id": spec_id,
                "markdown_bytes": markdown.len(),
                "if_match_set": if_match.is_some(),
            }),
            self.inner.append_to_spec(spec_id, markdown, if_match, jwt)
        )
    }

    async fn delete_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        with_recording_unit!(
            self,
            "delete_spec",
            json!({ "spec_id": spec_id }),
            self.inner.delete_spec(spec_id, jwt)
        )
    }

    // --- Tasks ----------------------------------------------------------
    async fn list_tasks(
        &self,
        project_id: &str,
        spec_id: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        self.inner.list_tasks(project_id, spec_id, jwt).await
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
        with_recording!(
            self,
            "create_task",
            json!({
                "project_id": project_id,
                "spec_id": spec_id,
                "title": title,
                "description_bytes": description.len(),
                "dependencies": dependencies,
                "order": order,
            }),
            self.inner.create_task(
                project_id,
                spec_id,
                title,
                description,
                dependencies,
                order,
                jwt
            )
        )
    }

    async fn update_task(
        &self,
        task_id: &str,
        updates: TaskUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        let args = json!({
            "task_id": task_id,
            "updates": {
                "title_set": updates.title.is_some(),
                "description_set": updates.description.is_some(),
                "status_set": updates.status.is_some(),
            },
        });
        with_recording!(
            self,
            "update_task",
            args,
            self.inner.update_task(task_id, updates, jwt)
        )
    }

    async fn delete_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        with_recording_unit!(
            self,
            "delete_task",
            json!({ "task_id": task_id }),
            self.inner.delete_task(task_id, jwt)
        )
    }

    async fn transition_task(
        &self,
        task_id: &str,
        status: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        with_recording!(
            self,
            "transition_task",
            json!({ "task_id": task_id, "status": status }),
            self.inner.transition_task(task_id, status, jwt)
        )
    }

    async fn claim_next_task(
        &self,
        project_id: &str,
        agent_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        with_recording!(
            self,
            "claim_next_task",
            json!({ "project_id": project_id, "agent_id": agent_id }),
            self.inner.claim_next_task(project_id, agent_id, jwt)
        )
    }

    async fn get_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        self.inner.get_task(task_id, jwt).await
    }

    // --- Project --------------------------------------------------------
    async fn get_project(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        self.inner.get_project(project_id, jwt).await
    }

    async fn update_project(
        &self,
        project_id: &str,
        updates: ProjectUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        let args = json!({
            "project_id": project_id,
            "updates": {
                "name_set": updates.name.is_some(),
                "description_set": updates.description.is_some(),
                "tech_stack_set": updates.tech_stack.is_some(),
                "build_command_set": updates.build_command.is_some(),
                "test_command_set": updates.test_command.is_some(),
            },
        });
        with_recording!(
            self,
            "update_project",
            args,
            self.inner.update_project(project_id, updates, jwt)
        )
    }

    // --- Storage (logs, stats) -----------------------------------------
    async fn create_log(
        &self,
        project_id: &str,
        message: &str,
        level: &str,
        agent_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let args = json!({
            "project_id": project_id,
            "level": level,
            "message_bytes": message.len(),
            "agent_id": agent_id,
            "has_metadata": metadata.is_some(),
        });
        with_recording!(
            self,
            "create_log",
            args,
            self.inner
                .create_log(project_id, message, level, agent_id, metadata, jwt)
        )
    }

    async fn list_logs(
        &self,
        project_id: &str,
        level: Option<&str>,
        limit: Option<u64>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner.list_logs(project_id, level, limit, jwt).await
    }

    async fn get_project_stats(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner.get_project_stats(project_id, jwt).await
    }

    // --- Messages -------------------------------------------------------
    async fn list_messages(
        &self,
        project_id: &str,
        instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>> {
        self.inner.list_messages(project_id, instance_id).await
    }

    async fn save_message(&self, params: SaveMessageParams) -> anyhow::Result<()> {
        let args = json!({
            "project_id": params.project_id,
            "instance_id": params.instance_id,
            "session_id": params.session_id,
            "role": params.role,
            "content_bytes": params.content.len(),
        });
        with_recording_unit!(self, "save_message", args, self.inner.save_message(params))
    }

    // --- Sessions -------------------------------------------------------
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor> {
        let args = json!({
            "instance_id": params.instance_id,
            "project_id": params.project_id,
            "model": params.model,
        });
        with_recording!(
            self,
            "create_session",
            args,
            self.inner.create_session(params)
        )
    }

    async fn get_active_session(
        &self,
        instance_id: &str,
    ) -> anyhow::Result<Option<SessionDescriptor>> {
        self.inner.get_active_session(instance_id).await
    }

    // --- Orbit / Network pass-through ----------------------------------
    //
    // These are generic verbs; classify via HTTP method. `GET` /
    // `HEAD` / `OPTIONS` are treated as read-only; everything else
    // routes through the kernel.
    async fn orbit_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        if is_read_only_http_method(method) {
            return self.inner.orbit_api_call(method, path, body, jwt).await;
        }
        let args = json!({
            "method": method,
            "path": path,
            "has_body": body.is_some(),
        });
        with_recording!(
            self,
            "orbit_api_call",
            args,
            self.inner.orbit_api_call(method, path, body, jwt)
        )
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
        if is_read_only_http_method(method) {
            return self.inner.network_api_call(method, path, body, jwt).await;
        }
        let args = json!({
            "method": method,
            "path": path,
            "has_body": body.is_some(),
        });
        with_recording!(
            self,
            "network_api_call",
            args,
            self.inner.network_api_call(method, path, body, jwt)
        )
    }
}

fn is_read_only_http_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS"
    )
}
