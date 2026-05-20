//! HTTP-backed `DomainApi` implementation.
//!
//! All routes use `Authorization: Bearer <jwt>` (user JWT from session).
//!
//! Phase C / Commit 4 relocates this from `aura-runtime` into the
//! standalone `aura-domain-http` crate so the gateway no longer owns
//! domain HTTP routing or depends on `reqwest` for domain calls.

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use aura_tools::domain_tools::{
    AgentInstanceDescriptor, CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor,
    ProjectUpdate, SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor,
    TaskUpdate,
};
use reqwest::Client;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

const MAX_CLOUDFLARE_RETRIES: u32 = 2;
const CLOUDFLARE_RETRY_BASE_MS: u64 = 1500;

fn is_cloudflare_block(status: reqwest::StatusCode, body: &str) -> bool {
    (status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
        && body.contains("<!DOCTYPE html")
}

pub struct HttpDomainApi {
    http: Client,
    storage_url: String,
    network_url: String,
    orbit_url: String,
    /// Optional `aura-os-server` base URL. When set, it replaces
    /// [`Self::storage_url`] as the base for spec / task / project /
    /// log routes so those writes hit `aura-os-server` and fire its
    /// side effects (disk mirror of spec markdown to
    /// `<workspace_root>/spec/<slug>.md`, SSE broadcast on the project
    /// stream, JWT billing header injection). Orbit / feed / billing
    /// routes are unaffected — they keep using their existing direct
    /// base URLs because `aura-os-server` does not proxy them.
    ///
    /// Sourced from `NodeConfig::aura_os_server_url` (env var
    /// `AURA_OS_SERVER_URL`). `None` preserves the historical
    /// behavior of posting directly to `aura-storage`.
    os_server_url: Option<String>,
}

impl HttpDomainApi {
    /// Build a new HTTP-backed domain API.
    ///
    /// `os_server_url` is the optional `aura-os-server` base URL. When
    /// `Some`, spec / task / project / log routes go through it so
    /// `aura-os-server`'s disk-mirror + SSE + JWT-billing side effects
    /// fire on every write. When `None` those routes fall back to
    /// `storage_url` (pre-`aura-os-server` behavior). Operators enable
    /// the override by setting `AURA_OS_SERVER_URL` on the node.
    ///
    /// # Errors
    /// Returns an error if `reqwest` fails to construct its HTTP client
    /// (typically a TLS backend initialization failure in constrained
    /// environments).
    pub fn new(
        storage_url: &str,
        network_url: &str,
        orbit_url: &str,
        os_server_url: Option<String>,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build HTTP client for HttpDomainApi")?;
        Ok(Self {
            http,
            storage_url: storage_url.trim_end_matches('/').to_string(),
            network_url: network_url.trim_end_matches('/').to_string(),
            orbit_url: orbit_url.trim_end_matches('/').to_string(),
            os_server_url: os_server_url.map(|u| u.trim_end_matches('/').to_string()),
        })
    }

    /// Base URL for routes `aura-os-server` owns (specs, tasks,
    /// projects, logs, project stats).
    ///
    /// Returns `os_server_url` when configured, otherwise falls back
    /// to `storage_url`. Kept as a helper so every spec/task/log
    /// handler funnels through a single override point — threading the
    /// new URL is a one-line change per call site and an operator who
    /// leaves `AURA_OS_SERVER_URL` unset keeps the historical direct
    /// `aura-storage` path verbatim.
    fn specs_tasks_base_url(&self) -> &str {
        self.os_server_url.as_deref().unwrap_or(&self.storage_url)
    }

    /// Base URL for project GET / PUT routes.
    ///
    /// Unlike specs / tasks / logs / stats, `/api/projects/:id` is not
    /// a route aura-storage exposes — it lives on aura-network (and,
    /// when the override is set, aura-os-server). Routing the fallback
    /// through `storage_url` therefore 404s every `get_project` call
    /// the dev-loop makes before it can run a single task. Keep this
    /// helper separate from `specs_tasks_base_url` so the project path
    /// lands on `network_url` when `AURA_OS_SERVER_URL` is unset, which
    /// is the pre-`feat(domain): route DomainApi writes through
    /// aura-os-server` behavior that every prior operator setup relied
    /// on.
    fn project_base_url(&self) -> &str {
        self.os_server_url.as_deref().unwrap_or(&self.network_url)
    }

    // -------------------------------------------------------------------------
    // JWT helpers (Authorization: Bearer, for /api/ routes)
    // -------------------------------------------------------------------------

    fn require_jwt(jwt: Option<&str>) -> anyhow::Result<&str> {
        jwt.ok_or_else(|| anyhow!("JWT required for this operation but not provided — ensure the front-end sends a token in RuntimeRequest.auth_jwt"))
    }

    async fn send_with_retry(
        &self,
        method: &str,
        url: &str,
        jwt: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<String> {
        for attempt in 0..=MAX_CLOUDFLARE_RETRIES {
            let req = match method {
                "POST" => self.http.post(url).bearer_auth(jwt),
                "PUT" => self.http.put(url).bearer_auth(jwt),
                "DELETE" => self.http.delete(url).bearer_auth(jwt),
                _ => self.http.get(url).bearer_auth(jwt),
            };
            let req = if let Some(b) = body { req.json(b) } else { req };

            let resp = req
                .send()
                .await
                .with_context(|| format!("{method} {url}"))?;
            let status = resp.status();
            let text = resp.text().await?;

            if status.is_success() {
                return Ok(text);
            }

            if is_cloudflare_block(status, &text) && attempt < MAX_CLOUDFLARE_RETRIES {
                let backoff = CLOUDFLARE_RETRY_BASE_MS * u64::from(2u32.pow(attempt));
                warn!(
                    url,
                    attempt,
                    backoff_ms = backoff,
                    "Cloudflare block detected, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                continue;
            }

            let truncated: String = if is_cloudflare_block(status, &text) {
                format!("Cloudflare is blocking requests to {url} — the service may be cold-starting or temporarily unavailable")
            } else {
                text.chars().take(300).collect()
            };
            return Err(anyhow!("HTTP {status}: {truncated}"));
        }
        unreachable!()
    }

    async fn api_get<T: DeserializeOwned>(&self, url: &str, jwt: &str) -> anyhow::Result<T> {
        debug!(url, "HttpDomainApi api GET");
        let text = self.send_with_retry("GET", url, jwt, None).await?;
        serde_json::from_str(&text).with_context(|| format!("parse response from {url}"))
    }

    async fn api_post<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
        jwt: &str,
    ) -> anyhow::Result<T> {
        debug!(url, "HttpDomainApi api POST");
        let text = self.send_with_retry("POST", url, jwt, Some(body)).await?;
        serde_json::from_str(&text).with_context(|| format!("parse response from {url}"))
    }

    async fn api_put<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
        jwt: &str,
    ) -> anyhow::Result<T> {
        debug!(url, "HttpDomainApi api PUT");
        let text = self.send_with_retry("PUT", url, jwt, Some(body)).await?;
        serde_json::from_str(&text).with_context(|| format!("parse response from {url}"))
    }

    async fn api_delete(&self, url: &str, jwt: &str) -> anyhow::Result<()> {
        debug!(url, "HttpDomainApi api DELETE");
        self.send_with_retry("DELETE", url, jwt, None).await?;
        Ok(())
    }
}

#[async_trait]
impl DomainApi for HttpDomainApi {
    // -- Specs (aura-storage, JWT /api/) --------------------------------------

    async fn list_specs(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/specs",
            self.specs_tasks_base_url()
        );
        self.api_get(&url, jwt).await
    }

    async fn get_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/specs/{spec_id}", self.specs_tasks_base_url());
        self.api_get(&url, jwt).await
    }

    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/specs",
            self.specs_tasks_base_url()
        );
        let body = serde_json::json!({
            "title": title,
            "markdownContents": content,
            "orderIndex": order,
        });
        self.api_post(&url, &body, jwt).await
    }

    async fn update_spec(
        &self,
        spec_id: &str,
        title: Option<&str>,
        content: Option<&str>,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/specs/{spec_id}", self.specs_tasks_base_url());
        let body = serde_json::json!({
            "title": title,
            "markdownContents": content,
            "ifMatch": if_match,
        });
        self.api_put(&url, &body, jwt).await
    }

    async fn update_spec_section(
        &self,
        spec_id: &str,
        section_heading: &str,
        new_body: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/specs/{spec_id}/section",
            self.specs_tasks_base_url()
        );
        let body = serde_json::json!({
            "sectionHeading": section_heading,
            "newBody": new_body,
            "ifMatch": if_match,
        });
        self.api_put(&url, &body, jwt).await
    }

    async fn append_to_spec(
        &self,
        spec_id: &str,
        markdown: &str,
        if_match: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/specs/{spec_id}/append", self.specs_tasks_base_url());
        let body = serde_json::json!({
            "markdown": markdown,
            "ifMatch": if_match,
        });
        self.api_post(&url, &body, jwt).await
    }

    async fn delete_spec(&self, spec_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/specs/{spec_id}", self.specs_tasks_base_url());
        self.api_delete(&url, jwt).await
    }

    // -- Tasks (aura-storage, JWT /api/) --------------------------------------

    async fn list_tasks(
        &self,
        project_id: &str,
        spec_id: Option<&str>,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        let jwt = Self::require_jwt(jwt)?;
        let mut url = format!(
            "{}/api/projects/{project_id}/tasks",
            self.specs_tasks_base_url()
        );
        if let Some(sid) = spec_id {
            use std::fmt::Write;
            let _ = write!(url, "?specId={sid}");
        }
        self.api_get(&url, jwt).await
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
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/tasks",
            self.specs_tasks_base_url()
        );
        let body = serde_json::json!({
            "specId": spec_id,
            "title": title,
            "description": description,
            "dependencyTaskIds": dependencies,
            "orderIndex": order,
        });
        self.api_post(&url, &body, jwt).await
    }

    async fn update_task(
        &self,
        task_id: &str,
        updates: TaskUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/tasks/{task_id}", self.specs_tasks_base_url());
        let body = serde_json::json!({
            "title": updates.title,
            "description": updates.description,
            "status": updates.status,
            "orderIndex": updates.order_index,
            "dependencyIds": updates.dependency_ids,
        });
        self.api_put(&url, &body, jwt).await
    }

    async fn delete_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<()> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/tasks/{task_id}", self.specs_tasks_base_url());
        self.api_delete(&url, jwt).await
    }

    async fn transition_task(
        &self,
        task_id: &str,
        status: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/tasks/{task_id}/transition",
            self.specs_tasks_base_url()
        );
        let body = serde_json::json!({ "status": status });
        self.api_post(&url, &body, jwt).await
    }

    async fn get_task(&self, task_id: &str, jwt: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/tasks/{task_id}", self.specs_tasks_base_url());
        self.api_get(&url, jwt).await
    }

    async fn claim_next_task(
        &self,
        project_id: &str,
        agent_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/tasks/claim?agentId={agent_id}",
            self.storage_url
        );
        let body = serde_json::json!({});
        match self.api_post::<TaskDescriptor>(&url, &body, jwt).await {
            Ok(t) => Ok(Some(t)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("404") || msg.contains("no task") || msg.contains("No task") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    // -- Project (aura-os-server when configured, else aura-network
    //    legacy fallback — aura-storage has no `/api/projects/:id`
    //    GET, only sub-routes, so falling back there 404s every
    //    `get_project` the dev-loop makes. Both endpoints on the
    //    override fire aura-os-server's SSE broadcast / JWT billing
    //    side effects, matching the spec/task write path.) --

    async fn get_project(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/projects/{project_id}", self.project_base_url());
        self.api_get(&url, jwt).await
    }

    async fn update_project(
        &self,
        project_id: &str,
        updates: ProjectUpdate,
        jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!("{}/api/projects/{project_id}", self.project_base_url());
        let body = serde_json::json!({
            "name": updates.name,
            "description": updates.description,
            "techStack": updates.tech_stack,
            "buildCommand": updates.build_command,
            "testCommand": updates.test_command,
        });
        self.api_put(&url, &body, jwt).await
    }

    // -- Project agents (aura-os-server) --------------------------------------
    //
    // Mirrors the marketplace `Hire` flow:
    //   POST /api/projects/{project_id}/agents { "agent_id": "..." }
    //   GET  /api/projects/{project_id}/agents
    // Routes through `project_base_url()` so they hit `aura-os-server` when
    // `AURA_OS_SERVER_URL` is configured and fall back to `aura-network`
    // otherwise — same routing rule as `get_project` / `update_project`.

    async fn list_project_agents(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<Vec<AgentInstanceDescriptor>> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/agents",
            self.project_base_url()
        );
        self.api_get(&url, jwt).await
    }

    async fn create_project_agent(
        &self,
        project_id: &str,
        agent_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<AgentInstanceDescriptor> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/agents",
            self.project_base_url()
        );
        let body = serde_json::json!({ "agent_id": agent_id });
        self.api_post(&url, &body, jwt).await
    }

    // -- Storage: logs (JWT /api/) --------------------------------------------

    async fn create_log(
        &self,
        project_id: &str,
        message: &str,
        level: &str,
        agent_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/projects/{project_id}/logs",
            self.specs_tasks_base_url()
        );
        let mut body = serde_json::json!({
            "message": message,
            "level": level,
        });
        if let Some(aid) = agent_id {
            body["projectAgentId"] = serde_json::Value::String(aid.to_string());
        }
        if let Some(meta) = metadata {
            body["metadata"] = meta.clone();
        }
        self.api_post(&url, &body, jwt).await
    }

    async fn list_logs(
        &self,
        project_id: &str,
        level: Option<&str>,
        limit: Option<u64>,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let jwt = Self::require_jwt(jwt)?;
        let mut url = format!(
            "{}/api/projects/{project_id}/logs",
            self.specs_tasks_base_url()
        );
        let mut params = Vec::new();
        if let Some(l) = level {
            params.push(format!("level={l}"));
        }
        if let Some(n) = limit {
            params.push(format!("limit={n}"));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        self.api_get(&url, jwt).await
    }

    async fn get_project_stats(
        &self,
        project_id: &str,
        jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let jwt = Self::require_jwt(jwt)?;
        let url = format!(
            "{}/api/stats?scope=project&projectId={project_id}",
            self.specs_tasks_base_url()
        );
        self.api_get(&url, jwt).await
    }

    // -- Messages / Sessions (not used by WS sessions) ------------------------

    async fn list_messages(
        &self,
        _project_id: &str,
        _instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>> {
        warn!("HttpDomainApi::list_messages not implemented");
        Ok(vec![])
    }

    async fn save_message(&self, _params: SaveMessageParams) -> anyhow::Result<()> {
        warn!("HttpDomainApi::save_message not implemented");
        Ok(())
    }

    async fn create_session(
        &self,
        _params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor> {
        Err(anyhow!("HttpDomainApi::create_session not implemented"))
    }

    async fn get_active_session(
        &self,
        _instance_id: &str,
    ) -> anyhow::Result<Option<SessionDescriptor>> {
        Ok(None)
    }

    // -- Orbit (raw JSON pass-through) ----------------------------------------

    async fn orbit_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("{}{path}", self.orbit_url);
        debug!(url, method, "HttpDomainApi orbit call");
        let mut req = match method {
            "POST" => self.http.post(&url),
            "PUT" => self.http.put(&url),
            "DELETE" => self.http.delete(&url),
            _ => self.http.get(&url),
        };
        if let Some(jwt) = jwt {
            req = req.bearer_auth(jwt);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let truncated: String = text.chars().take(500).collect();
            return Err(anyhow!("HTTP {status}: {truncated}"));
        }
        Ok(text)
    }

    fn orbit_url(&self) -> &str {
        &self.orbit_url
    }

    // -- Network (raw JSON pass-through) --------------------------------------

    async fn network_api_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("{}{path}", self.network_url);
        debug!(url, method, "HttpDomainApi network call");
        let mut req = match method {
            "POST" => self.http.post(&url),
            "PUT" => self.http.put(&url),
            _ => self.http.get(&url),
        };
        if let Some(jwt) = jwt {
            req = req.bearer_auth(jwt);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let truncated: String = text.chars().take(500).collect();
            return Err(anyhow!("HTTP {status}: {truncated}"));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `specs_tasks_base_url()` must prefer the aura-os-server override
    /// when set. This is the routing hook that lets an operator flip
    /// `AURA_OS_SERVER_URL` and redirect spec / task / project / log
    /// writes through aura-os-server so its disk-mirror + SSE + JWT
    /// billing side effects fire.
    #[test]
    fn specs_tasks_base_url_prefers_os_server_when_set() {
        let api = HttpDomainApi::new(
            "https://storage.example.com",
            "https://network.example.com",
            "https://orbit.example.com",
            Some("http://os".to_string()),
        )
        .expect("build HttpDomainApi");

        assert_eq!(api.specs_tasks_base_url(), "http://os");

        // Sanity-check URL composition for the highest-value route
        // (spec create) to lock in the exact path shape the rest of
        // the code assumes.
        let url = format!(
            "{}/api/projects/{pid}/specs",
            api.specs_tasks_base_url(),
            pid = "proj-1",
        );
        assert_eq!(url, "http://os/api/projects/proj-1/specs");
    }

    /// With no override, routing must fall back to `aura_storage_url`
    /// verbatim so existing deployments that haven't set
    /// `AURA_OS_SERVER_URL` see no behavior change.
    #[test]
    fn specs_tasks_base_url_falls_back_to_storage_when_unset() {
        let api = HttpDomainApi::new(
            "https://storage.example.com",
            "https://network.example.com",
            "https://orbit.example.com",
            None,
        )
        .expect("build HttpDomainApi");

        assert_eq!(api.specs_tasks_base_url(), "https://storage.example.com");

        let url = format!(
            "{}/api/projects/{pid}/specs",
            api.specs_tasks_base_url(),
            pid = "proj-1",
        );
        assert_eq!(url, "https://storage.example.com/api/projects/proj-1/specs");
    }

    /// Trailing slashes on either base URL must be normalised away so
    /// `format!("{base}/api/...")` never produces a `//api/...` path
    /// that some reverse proxies reject. Regression gate for the
    /// override — if we forget to trim we'd silently break every route
    /// that runs through the helper.
    #[test]
    fn os_server_url_trims_trailing_slash() {
        let api = HttpDomainApi::new(
            "https://storage.example.com/",
            "https://network.example.com",
            "https://orbit.example.com",
            Some("http://os/".to_string()),
        )
        .expect("build HttpDomainApi");

        assert_eq!(api.specs_tasks_base_url(), "http://os");
    }

    /// `project_base_url()` prefers the aura-os-server override when
    /// set so project GET / PUT writes fire the server's SSE / billing
    /// side effects alongside specs and tasks.
    #[test]
    fn project_base_url_prefers_os_server_when_set() {
        let api = HttpDomainApi::new(
            "https://storage.example.com",
            "https://network.example.com",
            "https://orbit.example.com",
            Some("http://os".to_string()),
        )
        .expect("build HttpDomainApi");

        assert_eq!(api.project_base_url(), "http://os");
    }

    /// Without the override, project routes must fall back to
    /// `aura_network_url` (not `storage_url`). aura-storage has no
    /// `/api/projects/:id` GET — only `/api/projects/:id/{specs,tasks,
    /// artifacts,logs}` sub-routes — so a storage fallback 404s every
    /// `get_project` the dev-loop issues and no task ever runs. This
    /// test pins the pre-`feat(domain): route DomainApi writes through
    /// aura-os-server` behavior so a future base-URL refactor can't
    /// silently reintroduce that regression.
    #[test]
    fn project_base_url_falls_back_to_network_when_unset() {
        let api = HttpDomainApi::new(
            "https://storage.example.com",
            "https://network.example.com",
            "https://orbit.example.com",
            None,
        )
        .expect("build HttpDomainApi");

        assert_eq!(api.project_base_url(), "https://network.example.com");

        let url = format!(
            "{}/api/projects/{pid}",
            api.project_base_url(),
            pid = "proj-1",
        );
        assert_eq!(url, "https://network.example.com/api/projects/proj-1");
    }
}
