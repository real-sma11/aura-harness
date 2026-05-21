//! Network domain tool handlers.

use serde_json::{json, Value};
use tracing::debug;

use super::api::{DomainApi, ListMarketplaceAgentsParams};
use super::helpers::{domain_err, domain_ok, str_field};

fn parse_api_result(body: &str) -> Value {
    serde_json::from_str::<Value>(body).unwrap_or_else(|_| Value::String(body.to_owned()))
}

pub async fn post_to_feed(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: post_to_feed");
    let body = json!({
        "profileId": input["profile_id"].as_str().unwrap_or_default(),
        "title": input["title"].as_str().unwrap_or_default(),
        "summary": input["summary"].as_str(),
        "postType": input["post_type"].as_str().unwrap_or("post"),
        "agentId": input["agent_id"].as_str(),
        "userId": input["user_id"].as_str(),
        "metadata": input["metadata"],
    });
    let jwt = str_field(input, "jwt");
    match api
        .network_api_call("POST", "/api/posts", Some(&body), jwt.as_deref())
        .await
    {
        Ok(r) => domain_ok(json!({ "result": parse_api_result(&r) })),
        Err(e) => domain_err(e),
    }
}

pub async fn network_list_projects(
    api: &dyn DomainApi,
    _project_id: &str,
    input: &Value,
) -> String {
    debug!("domain_tools: network_list_projects");
    let org_id = input["org_id"].as_str().unwrap_or_default();
    let jwt = str_field(input, "jwt").unwrap_or_default();
    let path = format!("/api/projects?org_id={org_id}");
    match api.network_api_call("GET", &path, None, Some(&jwt)).await {
        Ok(r) => domain_ok(json!({ "result": parse_api_result(&r) })),
        Err(e) => domain_err(e),
    }
}

pub async fn network_get_project(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: network_get_project");
    let project_id = input["project_id"].as_str().unwrap_or_default();
    let jwt = str_field(input, "jwt").unwrap_or_default();
    let path = format!("/api/projects/{project_id}");
    match api.network_api_call("GET", &path, None, Some(&jwt)).await {
        Ok(r) => domain_ok(json!({ "result": parse_api_result(&r) })),
        Err(e) => domain_err(e),
    }
}

pub async fn check_budget(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: check_budget");
    let org_id = input["org_id"].as_str().unwrap_or_default();
    let jwt = str_field(input, "jwt");
    let path = format!("/api/orgs/{org_id}/budget");
    match api
        .network_api_call("GET", &path, None, jwt.as_deref())
        .await
    {
        Ok(r) => domain_ok(json!({ "result": parse_api_result(&r) })),
        Err(e) => domain_err(e),
    }
}

pub async fn record_usage(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: record_usage");
    let body = json!({
        "orgId": input["org_id"].as_str().unwrap_or_default(),
        "userId": input["user_id"].as_str().unwrap_or_default(),
        "inputTokens": input["input_tokens"].as_u64().unwrap_or(0),
        "outputTokens": input["output_tokens"].as_u64().unwrap_or(0),
        "agentId": input["agent_id"].as_str(),
        "model": input["model"].as_str(),
    });
    let jwt = str_field(input, "jwt");
    match api
        .network_api_call("POST", "/api/usage", Some(&body), jwt.as_deref())
        .await
    {
        Ok(r) => domain_ok(json!({ "result": parse_api_result(&r) })),
        Err(e) => domain_err(e),
    }
}

/// Allowed values for the marketplace `sort` query parameter, mirroring
/// `MarketplaceTrendingSort` in
/// `interface/src/apps/marketplace/marketplace-trending.ts`. We validate
/// at the tool layer so a typo from the model surfaces as a structured
/// `invalid_sort` error rather than reaching the server.
const VALID_MARKETPLACE_SORTS: &[&str] = &["trending", "latest", "revenue", "reputation"];

/// Server-side hard cap on `limit` (mirrors aura-os-server's enforcement).
/// We validate locally so the model gets clean feedback instead of an
/// HTTP 400 wrapped in a generic `create_failed`-style envelope.
const MARKETPLACE_LIMIT_CAP: u32 = 100;

/// List agents publicly listed in the marketplace. Read-only; safe for
/// any caller holding `Capability::ListAgents`.
///
/// Filters supported (all optional): `sort` (one of trending / latest /
/// revenue / reputation), `expertise` (slug, exact match — the server
/// does NOT support free-text keyword search today), `limit` (1..=100),
/// `offset` (pagination).
///
/// Returns `{ agents: [...], total }` where each agent is a slim
/// projection — name, role, description, expertise, tags,
/// completed_tasks, revenue_usd, reputation, creator_display_name,
/// listed_at. Heavy fields (icon, system_prompt, personality) are
/// excluded.
pub async fn list_agents_marketplace(
    api: &dyn DomainApi,
    _project_id: &str,
    input: &Value,
) -> String {
    debug!("domain_tools: list_agents_marketplace");
    let sort = str_field(input, "sort");
    if let Some(ref s) = sort {
        if !s.is_empty() && !VALID_MARKETPLACE_SORTS.contains(&s.as_str()) {
            return domain_err(format!(
                "invalid sort '{s}'; expected one of {VALID_MARKETPLACE_SORTS:?}"
            ));
        }
    }
    let expertise = str_field(input, "expertise");
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    if let Some(n) = limit {
        if n == 0 || n > MARKETPLACE_LIMIT_CAP {
            return domain_err(format!(
                "limit must be between 1 and {MARKETPLACE_LIMIT_CAP} inclusive (got {n})"
            ));
        }
    }
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());

    let params = ListMarketplaceAgentsParams {
        sort: sort.as_deref().filter(|s| !s.is_empty()),
        expertise: expertise.as_deref().filter(|s| !s.is_empty()),
        limit,
        offset,
    };
    let jwt = str_field(input, "jwt");
    match api.list_marketplace_agents(params, jwt.as_deref()).await {
        Ok(response) => domain_ok(json!({
            "agents": response.agents,
            "total": response.total,
        })),
        Err(e) => domain_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_tools::api::{
        CreateSessionParams, ListMarketplaceAgentsParams, ListMarketplaceAgentsResponse,
        MarketplaceAgentDescriptor, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
        SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Minimal `DomainApi` double for the marketplace handler. Records the
    /// last params passed to `list_marketplace_agents` so tests can assert
    /// the handler forwarded what they expected, and returns a configurable
    /// response. Every other trait method bails so a misrouted call shows
    /// up loudly.
    #[derive(Default)]
    struct MarketplaceMockApi {
        last_params: Mutex<Option<OwnedParams>>,
        result: Mutex<Option<Result<ListMarketplaceAgentsResponse, String>>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    struct OwnedParams {
        sort: Option<String>,
        expertise: Option<String>,
        limit: Option<u32>,
        offset: Option<u32>,
    }

    impl MarketplaceMockApi {
        fn with_response(response: ListMarketplaceAgentsResponse) -> Self {
            Self {
                last_params: Mutex::new(None),
                result: Mutex::new(Some(Ok(response))),
            }
        }
        fn with_error(err: &str) -> Self {
            Self {
                last_params: Mutex::new(None),
                result: Mutex::new(Some(Err(err.to_string()))),
            }
        }
        fn observed_params(&self) -> OwnedParams {
            self.last_params.lock().unwrap().clone().unwrap_or_default()
        }
    }

    #[async_trait]
    impl DomainApi for MarketplaceMockApi {
        async fn list_marketplace_agents(
            &self,
            params: ListMarketplaceAgentsParams<'_>,
            _jwt: Option<&str>,
        ) -> anyhow::Result<ListMarketplaceAgentsResponse> {
            *self.last_params.lock().unwrap() = Some(OwnedParams {
                sort: params.sort.map(str::to_string),
                expertise: params.expertise.map(str::to_string),
                limit: params.limit,
                offset: params.offset,
            });
            match self.result.lock().unwrap().clone() {
                Some(Ok(r)) => Ok(r),
                Some(Err(msg)) => anyhow::bail!(msg),
                None => anyhow::bail!("no result configured"),
            }
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

    fn agent(agent_id: &str, name: &str) -> MarketplaceAgentDescriptor {
        MarketplaceAgentDescriptor {
            agent_id: agent_id.into(),
            name: name.into(),
            role: "qa".into(),
            description: format!("{name} is a useful agent for testing"),
            expertise: vec!["backend".into()],
            tags: vec!["rust".into()],
            completed_tasks: 12,
            revenue_usd: 1234.5,
            reputation: 0.87,
            creator_display_name: "ACME".into(),
            listed_at: "2026-05-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn list_marketplace_returns_agents_with_description_and_total() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![agent("a1", "Alpha"), agent("a2", "Bravo")],
            total: 42,
        });
        let raw = list_agents_marketplace(&api, "proj-1", &json!({})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(true));
        assert_eq!(env["total"], json!(42));
        let agents = env["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 2);
        // Description must be present — answering the "can it return description?" question.
        assert_eq!(
            agents[0]["description"],
            json!("Alpha is a useful agent for testing")
        );
        assert_eq!(agents[0]["agent_id"], json!("a1"));
        assert_eq!(agents[0]["creator_display_name"], json!("ACME"));
    }

    #[tokio::test]
    async fn list_marketplace_threads_filters_to_api() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![],
            total: 0,
        });
        let _ = list_agents_marketplace(
            &api,
            "proj-1",
            &json!({"sort": "revenue", "expertise": "backend", "limit": 25, "offset": 50}),
        )
        .await;
        let observed = api.observed_params();
        assert_eq!(observed.sort.as_deref(), Some("revenue"));
        assert_eq!(observed.expertise.as_deref(), Some("backend"));
        assert_eq!(observed.limit, Some(25));
        assert_eq!(observed.offset, Some(50));
    }

    #[tokio::test]
    async fn list_marketplace_rejects_invalid_sort_at_tool_layer() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![],
            total: 0,
        });
        let raw = list_agents_marketplace(&api, "proj-1", &json!({"sort": "popularity"})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(
            env["error"].as_str().unwrap().contains("invalid sort"),
            "error must explain which sort value was wrong"
        );
        assert!(
            api.observed_params() == OwnedParams::default(),
            "validation must happen before the API call — server should not be touched on bad input"
        );
    }

    #[tokio::test]
    async fn list_marketplace_rejects_limit_over_cap() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![],
            total: 0,
        });
        let raw = list_agents_marketplace(&api, "proj-1", &json!({"limit": 101})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(env["error"].as_str().unwrap().contains("limit"));
    }

    #[tokio::test]
    async fn list_marketplace_rejects_zero_limit() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![],
            total: 0,
        });
        let raw = list_agents_marketplace(&api, "proj-1", &json!({"limit": 0})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(env["error"].as_str().unwrap().contains("limit"));
    }

    #[tokio::test]
    async fn list_marketplace_surfaces_api_failure_as_error_envelope() {
        let api = MarketplaceMockApi::with_error("HTTP 503 upstream unavailable");
        let raw = list_agents_marketplace(&api, "proj-1", &json!({})).await;
        let env = parse(&raw);
        assert_eq!(env["ok"], json!(false));
        assert!(env["error"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn list_marketplace_with_no_filters_threads_empty_params() {
        let api = MarketplaceMockApi::with_response(ListMarketplaceAgentsResponse {
            agents: vec![],
            total: 0,
        });
        let _ = list_agents_marketplace(&api, "proj-1", &json!({})).await;
        assert_eq!(api.observed_params(), OwnedParams::default());
    }
}
