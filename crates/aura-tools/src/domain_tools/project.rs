//! Project domain tool handlers.

use serde_json::{json, Value};
use tracing::debug;

use super::api::{DomainApi, ProjectUpdate};
use super::helpers::{domain_err, domain_err_with_code, domain_ok, require_str, str_field};

pub async fn get_project(api: &dyn DomainApi, project_id: &str, input: &Value) -> String {
    debug!(project_id, "domain_tools: get_project");
    let jwt = str_field(input, "jwt");
    match api.get_project(project_id, jwt.as_deref()).await {
        Ok(p) => domain_ok(json!({ "project": p })),
        Err(e) => domain_err(e),
    }
}

pub async fn update_project(api: &dyn DomainApi, project_id: &str, input: &Value) -> String {
    debug!(project_id, "domain_tools: update_project");
    let updates = ProjectUpdate {
        name: str_field(input, "name"),
        description: str_field(input, "description"),
        tech_stack: str_field(input, "tech_stack"),
        build_command: str_field(input, "build_command"),
        test_command: str_field(input, "test_command"),
    };
    let jwt = str_field(input, "jwt");
    match api
        .update_project(project_id, updates, jwt.as_deref())
        .await
    {
        Ok(p) => domain_ok(json!({ "project": p })),
        Err(e) => domain_err(e),
    }
}

/// Assign an existing template agent (by `agent_id`) to a project.
///
/// Pre-flights with `list_project_agents` to detect duplicates:
/// * If the agent is already attached but its instance was **archived** (a
///   soft, server-persisted hide), it is reactivated (`Archived -> Idle`) and
///   returned under `instance` with `reactivated: true` — re-hiring an archived
///   agent brings it back rather than erroring.
/// * If it is already attached and active, returns a structured
///   `already_assigned` envelope (carrying the existing `agent_instance_id`) so
///   the LLM can re-use the existing instance instead of re-hiring.
///
/// On a fresh assign the new `AgentInstanceDescriptor` is returned under the
/// `instance` key.
///
/// `project_id` is resolved by the executor (session-level fallback when the
/// LLM omits it from the call args) — same shape as every other project-scoped
/// domain tool.
pub async fn assign_agent_to_project(
    api: &dyn DomainApi,
    project_id: &str,
    input: &Value,
) -> String {
    debug!(project_id, "domain_tools: assign_agent_to_project");
    let agent_id = match require_str(input, "agent_id") {
        Ok(v) => v,
        Err(e) => return domain_err(e),
    };
    if project_id.is_empty() {
        return domain_err("project_id is required (and not resolvable from session)");
    }
    let jwt = str_field(input, "jwt");

    // Duplicate detection.
    match api.list_project_agents(project_id, jwt.as_deref()).await {
        Ok(existing) => {
            if let Some(dup) = existing.iter().find(|inst| inst.agent_id == agent_id) {
                // Already attached. If that instance was archived (a soft,
                // server-persisted hide), reactivate it (Archived -> Idle)
                // instead of dead-ending — re-hiring an archived agent should
                // bring it back rather than error.
                if dup.status.eq_ignore_ascii_case("archived") {
                    return match api
                        .update_project_agent_status(&dup.id, project_id, "idle", jwt.as_deref())
                        .await
                    {
                        Ok(instance) => {
                            domain_ok(json!({ "instance": instance, "reactivated": true }))
                        }
                        Err(e) => domain_err_with_code(
                            "reactivate_failed",
                            e,
                            Some(json!({
                                "agent_instance_id": dup.id,
                                "agent_id": dup.agent_id,
                                "project_id": dup.project_id,
                            })),
                        ),
                    };
                }
                return domain_err_with_code(
                    "already_assigned",
                    format!(
                        "agent {agent_id} is already assigned to project {project_id} as instance {}",
                        dup.id
                    ),
                    Some(json!({
                        "agent_instance_id": dup.id,
                        "agent_id": dup.agent_id,
                        "project_id": dup.project_id,
                    })),
                );
            }
        }
        Err(e) => {
            // Pre-flight failed. Surface a distinct code so the LLM can
            // distinguish "couldn't check" from "found a duplicate".
            return domain_err_with_code("preflight_failed", e, None);
        }
    }

    match api
        .create_project_agent(project_id, &agent_id, jwt.as_deref())
        .await
    {
        Ok(instance) => domain_ok(json!({ "instance": instance })),
        Err(e) => {
            // Translate the most common server-side failure mode into a code
            // the LLM can branch on; everything else falls through to a
            // generic create_failed envelope.
            let msg = e.to_string();
            let code = if msg.contains("404") || msg.to_lowercase().contains("not found") {
                "template_not_found"
            } else {
                "create_failed"
            };
            domain_err_with_code(code, msg, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_tools::api::{
        AgentInstanceDescriptor, CreateSessionParams, MessageDescriptor, ProjectDescriptor,
        SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Minimal `DomainApi` test double for assign_agent_to_project.
    /// Only the methods the handler touches are populated; every other
    /// trait method bails so a misrouted call surfaces immediately.
    #[derive(Default)]
    struct AssignMockApi {
        existing: Mutex<Vec<AgentInstanceDescriptor>>,
        create_result: Mutex<Option<Result<AgentInstanceDescriptor, String>>>,
        list_should_fail: Mutex<Option<String>>,
    }

    impl AssignMockApi {
        fn with_existing(existing: Vec<AgentInstanceDescriptor>) -> Self {
            Self {
                existing: Mutex::new(existing),
                create_result: Mutex::new(None),
                list_should_fail: Mutex::new(None),
            }
        }
        fn with_create_ok(instance: AgentInstanceDescriptor) -> Self {
            Self {
                existing: Mutex::new(vec![]),
                create_result: Mutex::new(Some(Ok(instance))),
                list_should_fail: Mutex::new(None),
            }
        }
        fn with_create_err(err: &str) -> Self {
            Self {
                existing: Mutex::new(vec![]),
                create_result: Mutex::new(Some(Err(err.to_string()))),
                list_should_fail: Mutex::new(None),
            }
        }
        fn with_list_failure(err: &str) -> Self {
            Self {
                existing: Mutex::new(vec![]),
                create_result: Mutex::new(None),
                list_should_fail: Mutex::new(Some(err.to_string())),
            }
        }
    }

    #[async_trait]
    impl DomainApi for AssignMockApi {
        // -- methods exercised by these tests --------------------------------
        async fn list_project_agents(
            &self,
            _project_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<Vec<AgentInstanceDescriptor>> {
            if let Some(err) = self.list_should_fail.lock().unwrap().as_ref() {
                anyhow::bail!(err.clone());
            }
            Ok(self.existing.lock().unwrap().clone())
        }
        async fn create_project_agent(
            &self,
            project_id: &str,
            agent_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<AgentInstanceDescriptor> {
            match self.create_result.lock().unwrap().clone() {
                Some(Ok(mut inst)) => {
                    if inst.project_id.is_empty() {
                        inst.project_id = project_id.to_string();
                    }
                    if inst.agent_id.is_empty() {
                        inst.agent_id = agent_id.to_string();
                    }
                    Ok(inst)
                }
                Some(Err(msg)) => anyhow::bail!(msg),
                None => anyhow::bail!("create_project_agent called but no result configured"),
            }
        }
        async fn update_project_agent_status(
            &self,
            agent_instance_id: &str,
            project_id: &str,
            status: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<AgentInstanceDescriptor> {
            // Echo the requested status back on the addressed instance so the
            // reactivation path can assert the Archived -> Idle flip.
            Ok(AgentInstanceDescriptor {
                id: agent_instance_id.to_string(),
                agent_id: String::new(),
                project_id: project_id.to_string(),
                name: String::new(),
                status: status.to_string(),
            })
        }

        // -- everything else bails to catch accidental routing ----------------
        async fn list_specs(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<SpecDescriptor>> {
            anyhow::bail!("unused")
        }
        async fn get_spec(&self, _: &str, _: Option<&str>) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("unused")
        }
        async fn create_spec(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: u32,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_spec(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("unused")
        }
        async fn delete_spec(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn list_tasks(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<TaskDescriptor>> {
            anyhow::bail!("unused")
        }
        async fn create_task(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
            _: u32,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_task(
            &self,
            _: &str,
            _: TaskUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn delete_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn transition_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn claim_next_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<Option<TaskDescriptor>> {
            anyhow::bail!("unused")
        }
        async fn get_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn get_project(&self, _: &str, _: Option<&str>) -> anyhow::Result<ProjectDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_project(
            &self,
            _: &str,
            _: ProjectUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            anyhow::bail!("unused")
        }
        async fn create_log(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            anyhow::bail!("unused")
        }
        async fn list_logs(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<u64>,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            anyhow::bail!("unused")
        }
        async fn get_project_stats(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            anyhow::bail!("unused")
        }
        async fn list_messages(&self, _: &str, _: &str) -> anyhow::Result<Vec<MessageDescriptor>> {
            anyhow::bail!("unused")
        }
        async fn save_message(&self, _: SaveMessageParams) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn create_session(
            &self,
            _: CreateSessionParams,
        ) -> anyhow::Result<SessionDescriptor> {
            anyhow::bail!("unused")
        }
        async fn get_active_session(&self, _: &str) -> anyhow::Result<Option<SessionDescriptor>> {
            anyhow::bail!("unused")
        }
        async fn orbit_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("unused")
        }
        async fn network_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("unused")
        }
    }

    fn parse(raw: &str) -> Value {
        serde_json::from_str::<Value>(raw).expect("tool result must be JSON")
    }

    fn instance(id: &str, agent_id: &str, project_id: &str) -> AgentInstanceDescriptor {
        AgentInstanceDescriptor {
            id: id.into(),
            agent_id: agent_id.into(),
            project_id: project_id.into(),
            name: format!("Agent {id}"),
            status: "idle".into(),
        }
    }

    #[tokio::test]
    async fn assign_agent_returns_instance_on_success() {
        let api = AssignMockApi::with_create_ok(instance("inst-new", "tmpl-x", "proj-1"));
        let raw = assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(true));
        assert_eq!(env["instance"]["agent_instance_id"], json!("inst-new"));
        assert_eq!(env["instance"]["agent_id"], json!("tmpl-x"));
        assert_eq!(env["instance"]["project_id"], json!("proj-1"));
    }

    #[tokio::test]
    async fn assign_agent_detects_duplicate_with_existing_instance_id() {
        // Template already present in the project under instance inst-old.
        let api = AssignMockApi::with_existing(vec![instance("inst-old", "tmpl-x", "proj-1")]);
        let raw = assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert_eq!(env["error_code"], json!("already_assigned"));
        assert_eq!(
            env["agent_instance_id"],
            json!("inst-old"),
            "duplicate envelope must carry the existing instance id so the LLM can re-use it"
        );
        assert_eq!(env["agent_id"], json!("tmpl-x"));
        assert_eq!(env["project_id"], json!("proj-1"));
    }

    #[tokio::test]
    async fn assign_agent_reactivates_archived_duplicate() {
        // Template already attached, but its instance is archived. Re-hiring
        // must reactivate it (Archived -> Idle) and succeed, not return
        // already_assigned.
        let archived = AgentInstanceDescriptor {
            id: "inst-arch".into(),
            agent_id: "tmpl-x".into(),
            project_id: "proj-1".into(),
            name: "Agent inst-arch".into(),
            status: "archived".into(),
        };
        let api = AssignMockApi::with_existing(vec![archived]);
        let raw = assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(true), "archived duplicate must reactivate, not error");
        assert_eq!(env["reactivated"], json!(true));
        assert_eq!(env["instance"]["agent_instance_id"], json!("inst-arch"));
        assert_eq!(
            env["instance"]["status"], json!("idle"),
            "reactivation must flip the archived instance back to idle"
        );
    }

    #[tokio::test]
    async fn assign_agent_returns_template_not_found_on_404() {
        let api = AssignMockApi::with_create_err("HTTP 404 Not Found: agent template not found");
        let raw =
            assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-missing" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert_eq!(env["error_code"], json!("template_not_found"));
    }

    #[tokio::test]
    async fn assign_agent_distinguishes_preflight_failure() {
        let api = AssignMockApi::with_list_failure("HTTP 500 Internal Server Error");
        let raw = assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert_eq!(
            env["error_code"], json!("preflight_failed"),
            "list_project_agents failing must not be conflated with already_assigned or template_not_found"
        );
    }

    #[tokio::test]
    async fn assign_agent_rejects_missing_agent_id() {
        let api = AssignMockApi::default();
        let raw = assign_agent_to_project(&api, "proj-1", &json!({})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(
            env["error"].as_str().unwrap().contains("agent_id"),
            "error must mention the missing field by name"
        );
    }

    #[tokio::test]
    async fn assign_agent_rejects_empty_project_id() {
        let api = AssignMockApi::default();
        let raw = assign_agent_to_project(&api, "", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(env["error"].as_str().unwrap().contains("project_id"));
    }

    #[tokio::test]
    async fn assign_agent_does_not_collide_with_unrelated_template() {
        // A different template is already in the project — must not trigger
        // the duplicate path for our agent_id.
        let api = AssignMockApi {
            existing: Mutex::new(vec![instance("inst-other", "tmpl-y", "proj-1")]),
            create_result: Mutex::new(Some(Ok(instance("inst-new", "tmpl-x", "proj-1")))),
            list_should_fail: Mutex::new(None),
        };
        let raw = assign_agent_to_project(&api, "proj-1", &json!({ "agent_id": "tmpl-x" })).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(true));
        assert_eq!(env["instance"]["agent_instance_id"], json!("inst-new"));
    }
}
