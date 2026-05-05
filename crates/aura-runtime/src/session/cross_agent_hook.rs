use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use aura_core::AgentId;
use aura_kernel::{ChildAgentSpec, KernelSpawnHook, SpawnError, SpawnHook, SpawnOutcome};
use aura_store::Store;
use aura_tools::{AgentControlHook, AgentReadHook};

const CHAT_PERSISTED_HEADER: &str = "x-aura-chat-persisted";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime bridge from harness-native cross-agent tools back into aura-os.
///
/// The aura-tools crate owns permission gates. This hook owns the production
/// side effect: call aura-os-server with the session user's JWT so the target
/// agent receives the same chat turn the UI endpoint would have sent.
pub(crate) struct AuraServerAgentHook {
    base_url: String,
    auth_token: Option<String>,
    client: reqwest::Client,
}

pub(crate) struct AuraServerSpawnHook {
    base_url: String,
    auth_token: Option<String>,
    org_id: Option<String>,
    client: reqwest::Client,
    kernel: KernelSpawnHook,
}

impl AuraServerAgentHook {
    pub(crate) fn new(base_url: impl Into<String>, auth_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth_token,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn bearer(&self) -> Result<&str, String> {
        self.auth_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| "missing bearer token for aura-os-server callback".to_string())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn error_from_response(&self, prefix: &str, response: reqwest::Response) -> String {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Ok(api_error) = serde_json::from_str::<AuraOsApiError>(&body) {
            let details = api_error
                .details
                .or(api_error.error)
                .unwrap_or_else(|| "aura-os-server returned an error".to_string());
            return format!("{prefix}: {}: {details}", api_error.code);
        }
        if body.trim().is_empty() {
            format!("{prefix}: aura-os-server returned HTTP {status}")
        } else {
            format!("{prefix}: aura-os-server returned HTTP {status}: {body}")
        }
    }
}

impl AuraServerSpawnHook {
    pub(crate) fn new(
        base_url: impl Into<String>,
        auth_token: Option<String>,
        org_id: Option<String>,
        store: Arc<dyn Store>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth_token,
            org_id,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            kernel: KernelSpawnHook::new(store),
        }
    }

    fn bearer(&self) -> Result<&str, SpawnError> {
        self.auth_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                SpawnError::Other("missing bearer token for aura-os-server callback".into())
            })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn create_aura_os_agent(&self, child: &ChildAgentSpec) -> Result<String, SpawnError> {
        let response = self
            .client
            .post(self.endpoint("/api/agents"))
            .bearer_auth(self.bearer()?)
            .json(&json!({
                "org_id": self.org_id,
                "name": child.name,
                "role": child.role,
                "personality": "",
                "system_prompt": child.system_prompt_override.clone().unwrap_or_default(),
                "skills": [],
                "icon": null,
                "machine_type": "swarm",
                "adapter_type": "aura_harness",
                "permissions": child.permissions,
            }))
            .send()
            .await
            .map_err(|e| {
                SpawnError::Other(format!("spawn_agent: aura-os-server callback failed: {e}"))
            })?;
        if !response.status().is_success() {
            let message = AuraServerAgentHook {
                base_url: self.base_url.clone(),
                auth_token: self.auth_token.clone(),
                client: self.client.clone(),
            }
            .error_from_response("spawn_agent", response)
            .await;
            return Err(SpawnError::Other(message));
        }
        let body = response.json::<Value>().await.map_err(|e| {
            SpawnError::Other(format!("spawn_agent: invalid aura-os-server JSON: {e}"))
        })?;
        body.get("agent_id")
            .or_else(|| body.get("agentId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                SpawnError::Other("spawn_agent: aura-os-server response missing agent_id".into())
            })
    }
}

#[async_trait]
impl SpawnHook for AuraServerSpawnHook {
    async fn spawn_child(
        &self,
        parent_agent_id: &AgentId,
        originating_user_id: Option<&str>,
        mut child: ChildAgentSpec,
    ) -> Result<SpawnOutcome, SpawnError> {
        let external_agent_id = self.create_aura_os_agent(&child).await?;
        if let Ok(uuid) = uuid::Uuid::parse_str(&external_agent_id) {
            child.preassigned_agent_id = Some(AgentId::from_uuid(uuid));
        }
        let mut outcome = self
            .kernel
            .spawn_child(parent_agent_id, originating_user_id, child)
            .await?;
        outcome.external_agent_id = Some(external_agent_id);
        Ok(outcome)
    }
}

#[async_trait]
impl AgentControlHook for AuraServerAgentHook {
    async fn deliver_message(
        &self,
        target_agent_id: &str,
        _parent_agent_id: Option<&str>,
        _originating_user_id: Option<&str>,
        content: &str,
        attachments: Option<Value>,
    ) -> Result<(), String> {
        let url = self.endpoint(&format!("/api/agents/{target_agent_id}/events/stream"));
        let response = self
            .client
            .post(url)
            .bearer_auth(self.bearer()?)
            .json(&json!({
                "content": content,
                "action": null,
                "model": null,
                "commands": null,
                "project_id": null,
                "attachments": attachments,
                "new_session": false
            }))
            .send()
            .await
            .map_err(|e| format!("send_to_agent: aura-os-server callback failed: {e}"))?;

        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            return Err(self.error_from_response("send_to_agent", response).await);
        }
        require_persisted_header(&headers)
    }

    async fn lifecycle(
        &self,
        target_agent_id: &str,
        _parent_agent_id: Option<&str>,
        _originating_user_id: Option<&str>,
        action: &str,
    ) -> Result<(), String> {
        let actual_action = match action {
            "pause" => "hibernate",
            "resume" => "wake",
            other => other,
        };
        let url = self.endpoint(&format!(
            "/api/agents/{target_agent_id}/remote_agent/{actual_action}"
        ));
        let response = self
            .client
            .post(url)
            .bearer_auth(self.bearer()?)
            .send()
            .await
            .map_err(|e| format!("agent_lifecycle: aura-os-server callback failed: {e}"))?;
        if !response.status().is_success() {
            return Err(self.error_from_response("agent_lifecycle", response).await);
        }
        Ok(())
    }

    async fn delegate_task(
        &self,
        target_agent_id: &str,
        _parent_agent_id: Option<&str>,
        _originating_user_id: Option<&str>,
        task: &str,
        context: Option<&Value>,
    ) -> Result<(), String> {
        let url = self.endpoint(&format!("/api/agents/{target_agent_id}/delegate_task"));
        let response = self
            .client
            .post(url)
            .bearer_auth(self.bearer()?)
            .json(&json!({
                "task": task,
                "context": context
            }))
            .send()
            .await
            .map_err(|e| format!("delegate_task: aura-os-server callback failed: {e}"))?;
        if !response.status().is_success() {
            return Err(self.error_from_response("delegate_task", response).await);
        }

        self.deliver_message(
            target_agent_id,
            None,
            None,
            &format!("Delegated task:\n\n{task}"),
            context.cloned(),
        )
        .await
    }
}

#[async_trait]
impl AgentReadHook for AuraServerAgentHook {
    async fn list_agents(&self, org_id: Option<&str>) -> Result<Value, String> {
        // Always request the slim view: aura-os-server returns
        // `Vec<{agent_id, name, role}>` instead of full `Vec<Agent>`
        // (which carries multi-KB base64 WebP icons + system_prompt
        // + personality per row). The previous full payload routinely
        // exceeded the runner's 8000-char per-tool-result cap
        // (`MAX_COLLECTED_TOOL_RESULT_CHARS`), truncating the JSON
        // mid-record before the model could read agent names past
        // the first one or two. The aura-os-server `view=` knob is
        // documented at `apps/aura-os-server/src/handlers/agents/crud/list.rs`
        // (`AgentListView`); the cross-repo regression test in
        // `cross_agent_hook::tests` pins this query string so a future
        // refactor cannot silently re-bloat the tool result.
        let url = self.endpoint("/api/agents");
        let mut request = self
            .client
            .get(url)
            .bearer_auth(self.bearer()?)
            .query(&[("view", "slim")]);
        if let Some(org_id) = org_id {
            request = request.query(&[("org_id", org_id)]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("list_agents: aura-os-server callback failed: {e}"))?;
        if !response.status().is_success() {
            return Err(self.error_from_response("list_agents", response).await);
        }
        response
            .json::<Value>()
            .await
            .map_err(|e| format!("list_agents: invalid aura-os-server JSON: {e}"))
    }

    async fn snapshot(&self, target_agent_id: &str) -> Result<Value, String> {
        let url = self.endpoint(&format!("/api/agents/{target_agent_id}/state_snapshot"));
        let response = self
            .client
            .get(url)
            .bearer_auth(self.bearer()?)
            .send()
            .await
            .map_err(|e| format!("get_agent_state: aura-os-server callback failed: {e}"))?;
        if !response.status().is_success() {
            return Err(self.error_from_response("get_agent_state", response).await);
        }
        response
            .json::<Value>()
            .await
            .map_err(|e| format!("get_agent_state: invalid aura-os-server JSON: {e}"))
    }
}

fn require_persisted_header(headers: &HeaderMap) -> Result<(), String> {
    match headers
        .get(CHAT_PERSISTED_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some("true") => Ok(()),
        Some(value) => Err(format!(
            "send_to_agent: aura-os-server accepted the stream but reported {CHAT_PERSISTED_HEADER}: {value}"
        )),
        None => Err(format!(
            "send_to_agent: aura-os-server response missing {CHAT_PERSISTED_HEADER}"
        )),
    }
}

#[derive(Debug, Deserialize)]
struct AuraOsApiError {
    error: Option<String>,
    code: String,
    details: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::routing::get;
    use axum::Json;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[test]
    fn persisted_header_accepts_true() {
        let mut headers = HeaderMap::new();
        headers.insert(CHAT_PERSISTED_HEADER, HeaderValue::from_static("true"));
        assert!(require_persisted_header(&headers).is_ok());
    }

    #[test]
    fn persisted_header_rejects_false() {
        let mut headers = HeaderMap::new();
        headers.insert(CHAT_PERSISTED_HEADER, HeaderValue::from_static("false"));
        let err = require_persisted_header(&headers).unwrap_err();
        assert!(err.contains("reported"), "got: {err}");
    }

    #[test]
    fn persisted_header_rejects_missing() {
        let err = require_persisted_header(&HeaderMap::new()).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
    }

    /// Cross-repo regression: pin that `list_agents` always asks
    /// aura-os-server for the slim shape via `?view=slim`. Without
    /// this, the full `Vec<Agent>` payload (multi-KB icons +
    /// system_prompt per row) overflows the runner's per-tool-result
    /// cap and truncates the JSON before agent names are reachable —
    /// which manifested as "every list_agents call gets truncated
    /// before I reach the name fields" on the LLM side. The matching
    /// server endpoint is documented at
    /// `apps/aura-os-server/src/handlers/agents/crud/list.rs`
    /// (`AgentListView`); if either side drifts, this test fails first.
    async fn spawn_capturing_mock(
        captured: Arc<Mutex<Option<HashMap<String, String>>>>,
    ) -> String {
        let captured_for_route = captured.clone();
        let app = axum::Router::new().route(
            "/api/agents",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let captured = captured_for_route.clone();
                async move {
                    *captured.lock().await = Some(params);
                    Json(serde_json::json!([]))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn list_agents_callback_always_requests_view_slim() {
        let captured = Arc::new(Mutex::new(None::<HashMap<String, String>>));
        let base_url = spawn_capturing_mock(captured.clone()).await;

        let hook = AuraServerAgentHook::new(base_url, Some("test-jwt".into()));
        hook.list_agents(None).await.expect("list_agents call");

        let params = captured.lock().await.clone().expect("captured params");
        assert_eq!(
            params.get("view").map(String::as_str),
            Some("slim"),
            "list_agents must request view=slim from aura-os-server; got {params:?}"
        );
    }

    #[tokio::test]
    async fn list_agents_callback_combines_view_slim_with_org_id() {
        let captured = Arc::new(Mutex::new(None::<HashMap<String, String>>));
        let base_url = spawn_capturing_mock(captured.clone()).await;

        let hook = AuraServerAgentHook::new(base_url, Some("test-jwt".into()));
        hook.list_agents(Some("org-1"))
            .await
            .expect("list_agents call");

        let params = captured.lock().await.clone().expect("captured params");
        assert_eq!(
            params.get("view").map(String::as_str),
            Some("slim"),
            "view=slim must accompany org_id; got {params:?}"
        );
        assert_eq!(
            params.get("org_id").map(String::as_str),
            Some("org-1"),
            "org_id must be forwarded alongside view=slim; got {params:?}"
        );
    }
}
