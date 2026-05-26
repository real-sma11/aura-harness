//! Shared E2E test infrastructure.
//!
//! Provides `TestServer`, `WsClient`, and helper utilities used by both
//! `e2e_live` and `e2e_full` integration test suites.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aura_auth::CredentialStore;
use aura_kernel::Executor;
use aura_reasoner::{AnthropicConfig, AnthropicProvider, MockProvider, ModelProvider};
use aura_runtime::test_support::{create_router, RouterState, RouterStateConfig, Scheduler};
use aura_runtime::NodeConfig;
use aura_store::RocksStore;
use aura_tools::catalog::ToolProfile;
use aura_tools::{ToolCatalog, ToolConfig, ToolResolver};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;

/// Default full-access `agent_permissions` payload for `session_init`
/// messages.
pub(crate) fn default_agent_permissions_payload() -> Value {
    json!({
        "scope": { "orgs": [], "projects": [], "agent_ids": [] },
        "capabilities": [
            { "type": "spawnAgent" },
            { "type": "controlAgent" },
            { "type": "readAgent" },
            { "type": "listAgents" },
            { "type": "manageOrgMembers" },
            { "type": "manageBilling" },
            { "type": "invokeProcess" },
            { "type": "postToFeed" },
            { "type": "generateMedia" },
            { "type": "readAllProjects" },
            { "type": "writeAllProjects" }
        ]
    })
}

// ============================================================================
// Credential helpers
// ============================================================================

/// Resolve auth token for LLM tests. Panics when credentials are missing.
pub fn require_llm_token() -> String {
    load_auth_token().unwrap_or_else(|| {
        panic!(
            "LLM credentials required: no auth token available. \
             Set AURA_ROUTER_JWT or run `aura login`."
        )
    })
}

#[macro_export]
macro_rules! require_llm {
    () => {
        $crate::common::require_llm_token()
    };
}

/// Return (email, password) from E2E env vars when configured.
pub fn optional_zos_credentials() -> Option<(String, String)> {
    let email = std::env::var("E2E_ZOS_EMAIL").unwrap_or_default();
    let password = std::env::var("E2E_ZOS_PASSWORD").unwrap_or_default();
    if email.is_empty() || password.is_empty() {
        return None;
    }
    Some((email, password))
}

#[macro_export]
macro_rules! require_zos {
    () => {
        match $crate::common::optional_zos_credentials() {
            Some(credentials) => credentials,
            None => {
                eprintln!(
                    "skipping credentialed test: set E2E_ZOS_EMAIL and E2E_ZOS_PASSWORD to run it"
                );
                return;
            }
        }
    };
}

/// Perform a real ZOS login and return the JWT access token.
pub async fn zos_login(email: &str, password: &str) -> Result<String, String> {
    let client = aura_auth::ZosClient::new().map_err(|e| format!("ZosClient::new failed: {e}"))?;
    let session = client
        .login(email, password)
        .await
        .map_err(|e| format!("ZOS login failed: {e}"))?;
    Ok(session.access_token)
}

// ============================================================================
// TestServer
// ============================================================================

pub struct TestServer {
    base_url: String,
    _data_dir: tempfile::TempDir,
    _server_handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with_options(None).await
    }

    /// Boot a `TestServer` with bearer auth *disabled*, matching the
    /// default `AURA_NODE_REQUIRE_AUTH` behaviour. Used by
    /// [`crate::common::start_without_auth`] callers that want to
    /// verify the router still accepts unauthenticated requests when
    /// the gate is off.
    pub async fn start_without_auth() -> Self {
        Self::start_inner(None, false).await
    }

    pub async fn start_with_options(
        provider_override: Option<Arc<dyn ModelProvider + Send + Sync>>,
    ) -> Self {
        Self::start_inner(provider_override, true).await
    }

    async fn start_inner(
        provider_override: Option<Arc<dyn ModelProvider + Send + Sync>>,
        require_auth: bool,
    ) -> Self {
        let _ = dotenvy::dotenv();

        let data_dir = tempfile::tempdir().expect("create temp dir");

        let db_path = data_dir.path().join("db");
        let workspaces_path = data_dir.path().join("workspaces");
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::create_dir_all(&workspaces_path).unwrap();

        let mut config = NodeConfig::from_env();
        config.data_dir = data_dir.path().to_path_buf();
        // Phase 4 (security audit): the router's bearer middleware now
        // does a constant-time compare against `config.auth_token`.
        // Force the deterministic test value so `http_client()` and
        // `WsClient::connect()` (both of which send `E2E_TEST_BEARER`)
        // pass through, regardless of whether the ambient environment
        // has `AURA_NODE_AUTH_TOKEN` set to something else.
        //
        // Auth is *off by default* at the node level — tests that rely
        // on it have to opt in. `start_with_options` / `start` flip
        // `require_auth = true` so the router re-attaches
        // `require_bearer_mw`; [`Self::start_without_auth`] leaves the
        // gate closed to exercise the default path.
        config.require_auth = require_auth;
        if require_auth {
            config.auth_token = E2E_TEST_BEARER.to_string();
        } else {
            config.auth_token.clear();
        }

        let store: Arc<dyn aura_store::Store> =
            Arc::new(RocksStore::open(&db_path, false).expect("open rocks"));

        let tool_config = ToolConfig {
            command: aura_tools::CommandPolicy {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let catalog = Arc::new(ToolCatalog::new());
        let tools = catalog.visible_tools(ToolProfile::Core, &tool_config);
        let resolver: Arc<dyn Executor> =
            Arc::new(ToolResolver::new(catalog.clone(), tool_config.clone()));
        let executors = vec![resolver];

        let provider: Arc<dyn ModelProvider + Send + Sync> =
            provider_override.unwrap_or_else(create_provider);

        let scheduler = Arc::new(Scheduler::new(
            store.clone(),
            provider.clone(),
            executors,
            tools,
            workspaces_path,
            None,
        ));

        let state = RouterState::new(RouterStateConfig {
            store,
            scheduler,
            config,
            provider,
            tool_config,
            catalog,
            domain_api: None,
            automaton_controller: None,
            automaton_bridge: None,
            memory_manager: None,
            skill_manager: None,
            router_url: None,
        });
        let app = create_router(state);

        let bind = std::env::var("E2E_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:0".to_string());
        let addr: SocketAddr = bind.parse().expect("parse bind addr");
        let listener = TcpListener::bind(addr).await.expect("bind");
        let local_addr = listener.local_addr().unwrap();
        let base_url = format!("http://{local_addr}");

        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service()).await.ok();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        Self {
            base_url,
            _data_dir: data_dir,
            _server_handle: handle,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn ws_url(&self) -> String {
        self.base_url.replace("http://", "ws://") + "/stream"
    }

    pub fn workspaces_path(&self) -> PathBuf {
        self._data_dir.path().join("workspaces")
    }
}

pub fn create_provider() -> Arc<dyn ModelProvider + Send + Sync> {
    match AnthropicProvider::new(AnthropicConfig::from_env()) {
        Ok(p) => Arc::new(p),
        Err(_) => Arc::new(MockProvider::simple_response("(mock)")),
    }
}

/// Create a TestServer that always uses MockProvider.
pub async fn start_mock_server() -> TestServer {
    let mock: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("Hello from mock provider."));
    TestServer::start_with_options(Some(mock)).await
}

// ============================================================================
// WsClient
// ============================================================================

pub struct WsClient {
    pub write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMsg,
    >,
    pub read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
}

impl WsClient {
    /// Open a WebSocket to the node, attaching the default test Bearer
    /// header. The router `require_bearer_mw` middleware rejects WS
    /// upgrade requests without auth, so every `/stream*` connection
    /// now goes through this code path. Tests that want to exercise
    /// the unauthenticated path should use [`Self::connect_anonymous`].
    pub async fn connect(ws_url: &str) -> Self {
        Self::connect_with_auth(ws_url, E2E_TEST_BEARER).await
    }

    /// Open a WebSocket without an Authorization header. Only used by
    /// tests that deliberately verify the 401 rejection path.
    #[allow(dead_code)]
    pub async fn connect_anonymous(ws_url: &str) -> Self {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .expect("ws connect");
        let (write, read) = stream.split();
        Self { write, read }
    }

    pub async fn connect_with_auth(ws_url: &str, bearer_token: &str) -> Self {
        use tokio_tungstenite::tungstenite::http::Request;
        let req = Request::builder()
            .uri(ws_url)
            .header("Authorization", format!("Bearer {bearer_token}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .header("Host", "localhost")
            .body(())
            .unwrap();
        let (stream, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("ws connect with auth");
        let (write, read) = stream.split();
        Self { write, read }
    }

    pub async fn send_json(&mut self, msg: &Value) {
        let text = serde_json::to_string(msg).unwrap();
        self.write
            .send(WsMsg::Text(text.into()))
            .await
            .expect("ws send");
    }

    pub async fn send_session_init(&mut self, workspace: &Path, token: Option<&str>) {
        self.send_session_init_full(workspace, token, None, None, None, None)
            .await;
    }

    pub async fn send_session_init_full(
        &mut self,
        workspace: &Path,
        token: Option<&str>,
        system_prompt: Option<&str>,
        model: Option<&str>,
        max_tokens: Option<u32>,
        max_turns: Option<u32>,
    ) {
        let mut init = json!({
            "type": "session_init",
            "workspace": workspace.to_string_lossy(),
            "max_turns": max_turns.unwrap_or(aura_core::MAX_TURNS),
            "agent_permissions": default_agent_permissions_payload()
        });
        if let Some(t) = token {
            init["token"] = json!(t);
        }
        if let Some(sp) = system_prompt {
            init["system_prompt"] = json!(sp);
        }
        if let Some(m) = model {
            init["model"] = json!(m);
        }
        if let Some(mt) = max_tokens {
            init["max_tokens"] = json!(mt);
        }
        self.send_json(&init).await;
    }

    /// Send a session_init with all possible fields.
    pub async fn send_session_init_extended(
        &mut self,
        workspace: &Path,
        opts: SessionInitOpts<'_>,
    ) {
        let mut init = json!({
            "type": "session_init",
            "workspace": workspace.to_string_lossy(),
            "agent_permissions": default_agent_permissions_payload(),
        });
        if let Some(t) = opts.token {
            init["token"] = json!(t);
        }
        if let Some(sp) = opts.system_prompt {
            init["system_prompt"] = json!(sp);
        }
        if let Some(m) = opts.model {
            init["model"] = json!(m);
        }
        if let Some(mt) = opts.max_tokens {
            init["max_tokens"] = json!(mt);
        }
        if let Some(mt) = opts.max_turns {
            init["max_turns"] = json!(mt);
        }
        if let Some(temp) = opts.temperature {
            init["temperature"] = json!(temp);
        }
        if let Some(pid) = opts.project_id {
            init["project_id"] = json!(pid);
        }
        if let Some(pp) = opts.project_path {
            init["project_path"] = json!(pp);
        }
        if let Some(tools) = opts.installed_tools {
            init["installed_tools"] = json!(tools);
        }
        if let Some(integrations) = opts.installed_integrations {
            init["installed_integrations"] = json!(integrations);
        }
        if let Some(msgs) = opts.conversation_messages {
            init["conversation_messages"] = json!(msgs);
        }
        self.send_json(&init).await;
    }

    pub async fn send_user_message(&mut self, content: &str) {
        self.send_json(&json!({"type": "user_message", "content": content}))
            .await;
    }

    pub async fn send_cancel(&mut self) {
        self.send_json(&json!({"type": "cancel"})).await;
    }

    pub async fn send_raw(&mut self, raw: &str) {
        self.write
            .send(WsMsg::Text(raw.to_string().into()))
            .await
            .expect("ws send raw");
    }

    pub async fn recv_json(&mut self) -> Option<Value> {
        self.recv_json_timeout(Duration::from_secs(30)).await
    }

    pub async fn recv_json_timeout(&mut self, timeout: Duration) -> Option<Value> {
        match tokio::time::timeout(timeout, self.read.next()).await {
            Ok(Some(Ok(WsMsg::Text(text)))) => serde_json::from_str(text.as_ref()).ok(),
            _ => None,
        }
    }

    pub async fn expect_session_ready(&mut self) -> Value {
        let msg = self.recv_json().await.expect("expected session_ready");
        assert_eq!(
            msg["type"], "session_ready",
            "expected session_ready, got: {msg}"
        );
        msg
    }

    /// Collect all messages for one turn until `assistant_message_end` or timeout.
    pub async fn collect_turn(&mut self, timeout: Duration) -> Vec<Value> {
        let mut messages = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.recv_json_timeout(remaining).await {
                Some(msg) => {
                    let is_end = msg["type"] == "assistant_message_end";
                    let is_error =
                        msg["type"] == "error" && msg["recoverable"].as_bool() == Some(false);
                    messages.push(msg);
                    if is_end || is_error {
                        break;
                    }
                }
                None => break,
            }
        }
        messages
    }
}

/// Options bag for extended session_init.
#[derive(Default)]
pub struct SessionInitOpts<'a> {
    pub token: Option<&'a str>,
    pub system_prompt: Option<&'a str>,
    pub model: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub max_turns: Option<u32>,
    pub temperature: Option<f32>,
    pub project_id: Option<&'a str>,
    pub project_path: Option<&'a str>,
    pub installed_tools: Option<Vec<Value>>,
    pub installed_integrations: Option<Vec<Value>>,
    pub conversation_messages: Option<Vec<Value>>,
}

// ============================================================================
// Assertion / extraction helpers
// ============================================================================

/// Extract all tool names used in a turn's message stream.
pub fn tool_names_used(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m["type"] == "tool_use_start")
        .filter_map(|m| m["name"].as_str().map(String::from))
        .collect()
}

/// Concatenate all text_delta content in a turn.
pub fn collect_text(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|m| m["type"] == "text_delta")
        .filter_map(|m| m["text"].as_str())
        .collect()
}

/// Check that a turn ended with a given stop_reason.
pub fn assert_stop_reason(messages: &[Value], expected: &str) {
    let end = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end");
    assert!(end.is_some(), "no assistant_message_end found");
    assert_eq!(
        end.unwrap()["stop_reason"].as_str().unwrap(),
        expected,
        "unexpected stop_reason"
    );
}

/// Check that a turn contains a tool_result with is_error for a given tool.
pub fn has_tool_error(messages: &[Value], tool_name: &str) -> bool {
    messages.iter().any(|m| {
        m["type"] == "tool_result"
            && m["name"] == tool_name
            && m["is_error"].as_bool() == Some(true)
    })
}

// ============================================================================
// Auth helpers
// ============================================================================

/// Load an auth token from env or credential store.
pub fn load_auth_token() -> Option<String> {
    if let Ok(jwt) = std::env::var("AURA_ROUTER_JWT") {
        if !jwt.is_empty() {
            return Some(jwt);
        }
    }
    CredentialStore::load_token()
}

// ============================================================================
// Filesystem helpers
// ============================================================================

/// Find a file by name anywhere under a directory tree (deepest match first).
pub fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_file(&path, name) {
                    return Some(found);
                }
            }
        }
    }
    if dir.join(name).exists() {
        return Some(dir.join(name));
    }
    None
}

/// Find the agent subdirectory created by the session under the workspace.
pub fn find_agent_dir(ws_path: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(ws_path).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            return Some(path);
        }
    }
    None
}

/// Place a file in the agent workspace directory (where tools operate).
pub fn place_file_in_agent_dir(ws_path: &Path, name: &str, content: &str) {
    if let Some(agent_dir) = find_agent_dir(ws_path) {
        std::fs::write(agent_dir.join(name), content).unwrap();
    } else {
        std::fs::write(ws_path.join(name), content).unwrap();
    }
}

/// Bearer token used by the integration tests.
///
/// The router-wide `require_bearer_mw` middleware (security audit —
/// phase 1) rejects any non-`/health` request that doesn't carry a
/// non-empty Bearer token. These tests don't exercise the value of
/// the token (phase 4 will add a real shared secret), so we just need
/// a well-formed header — this constant is shared by `http_client`
/// and `WsClient::connect` to keep every caller honest.
pub const E2E_TEST_BEARER: &str = "test";

/// Create a reqwest HTTP client with the default test Bearer header.
///
/// Anything that hits `/tx`, `/agents/...`, etc. now requires auth.
/// Baking the header into the client keeps individual test functions
/// from sprinkling `.bearer_auth(...)` calls everywhere.
pub fn http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {E2E_TEST_BEARER}")).unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Convenience: connect a WS client with session_init + auth token.
pub async fn connect_llm_session(server: &TestServer, ws_path: &Path, token: &str) -> WsClient {
    let mut ws = WsClient::connect(&server.ws_url()).await;
    let tok = if token.is_empty() { None } else { Some(token) };
    ws.send_session_init(ws_path, tok).await;
    ws.expect_session_ready().await;
    ws
}
