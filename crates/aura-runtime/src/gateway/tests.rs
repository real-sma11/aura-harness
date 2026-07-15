use super::*;
use aura_agent::KernelModelGateway;
use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_context_memory::{
    ConsolidationConfig, MemoryAccessContext, MemoryManager, MemoryQueryContext, ProcedureConfig,
    RefinerConfig, RetrievalConfig, WriteConfig,
};
use aura_context_skills::{SkillInstallStore, SkillLoader, SkillManager};
use aura_core_types::AgentId;
use aura_model_reasoner::MockProvider;
use aura_store_db::RocksStore;
use axum::body::Body;
use axum::http::{request::Builder as RequestBuilder, Request};
use tower::util::ServiceExt;

/// Test bearer token injected by [`authed_request`].
///
/// Phase 4 of the security audit taught `require_bearer_mw` to do a
/// constant-time compare against [`NodeConfig::auth_token`], so the
/// value here is no longer cosmetic: it must match the configured
/// secret. [`NodeConfig::default`] ships with the same literal
/// (documented as a test-only placeholder) so every helper in this
/// file keeps working with zero boilerplate. Tests that deliberately
/// want the 401 path (e.g. [`test_requires_bearer_on_protected_routes`])
/// either omit the header or send a non-matching value.
const TEST_BEARER: &str = "test";

/// Build a [`Request`] pre-populated with an `Authorization: Bearer test`
/// header so the protected routes let us through.
///
/// Tests that explicitly want to exercise the unauthenticated path
/// (e.g. [`test_requires_bearer_on_protected_routes`]) bypass this and
/// use [`Request::builder`] directly.
fn authed_request() -> RequestBuilder {
    Request::builder().header("authorization", format!("Bearer {TEST_BEARER}"))
}

fn test_router_state(store: Arc<dyn Store>) -> RouterState {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("mock"));
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        std::path::PathBuf::from("/tmp/workspaces"),
        None,
    ));
    // require_auth = true keeps this router unit-test suite
    // exercising the full bearer-middleware path. NodeConfig::default
    // flipped the gate to false when AURA_NODE_REQUIRE_AUTH was
    // introduced, so we opt in locally - every test in this file that
    // expects a 401 (or sends a matching `Bearer test`) relies on
    // enforcement being active.
    let config = NodeConfig {
        require_auth: true,
        ..NodeConfig::default()
    };
    RouterState::new(crate::gateway::RouterStateConfig {
        store,
        scheduler,
        config,
        provider,
        tool_config: ToolConfig::default(),
        catalog: Arc::new(ToolCatalog::new()),
        domain_api: None,
        automaton_controller: None,
        automaton_bridge: None,
        memory_manager: None,
        skill_manager: None,
        secrets_vault: None,
        process_store: None,
        trigger_registrar: None,
        router_url: None,
    })
}

fn create_test_store() -> Arc<dyn Store> {
    let dir = tempfile::tempdir().unwrap();
    Arc::new(RocksStore::open(dir.path(), false).unwrap())
}

#[tokio::test]
async fn test_health_endpoint() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/health must remain reachable without a bearer token"
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json["version"].is_string());
    // git_sha is baked in via AURA_HARNESS_GIT_SHA in container images and
    // null for local builds; either way the key must be present.
    assert!(
        json.get("git_sha").is_some(),
        "/health must always include the git_sha key"
    );
    assert!(json["git_sha"].is_string() || json["git_sha"].is_null());
}

/// Verify that `/health` exposes the effective tool policy so the
/// `aura-os-desktop` `--external-harness` startup check can detect a
/// misconfigured external harness (the 3.0-class `run_command`
/// regression) without any authenticated probe. The test builds a
/// `RouterState` with a permissive `ToolConfig` and asserts each
/// policy field lands on the response with the same value.
#[tokio::test]
async fn test_health_endpoint_exposes_tool_policy_for_external_harness_probe() {
    let store = create_test_store();
    let mut state = test_router_state(store);
    state.tool_config = ToolConfig {
        command: aura_tools::CommandPolicy {
            enabled: true,
            command_allowlist: vec!["cargo".into(), "git".into()],
            binary_allowlist: vec!["cargo".into(), "git".into()],
            allow_shell: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = create_router(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(
        json["run_command_enabled"], true,
        "external-harness probe relies on this field"
    );
    assert_eq!(json["shell_enabled"], true);
    assert!(json["allowed_commands"].is_array());
    let allowed: Vec<String> = json["allowed_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(allowed, vec!["cargo", "git"]);
    assert!(json["binary_allowlist"].is_array());
    let binaries: Vec<String> = json["binary_allowlist"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(binaries, vec!["cargo", "git"]);
}

/// Symmetric negative test: a `ToolConfig::default()` — the
/// `aura-tools` crate's kernel-level fail-closed baseline — must still
/// report `run_command_enabled=false` over `/health` so operators who
/// deliberately wire the minimal executor can observe the narrowed
/// execution surface.
#[tokio::test]
async fn test_health_endpoint_reports_run_command_disabled_on_default_tool_config() {
    let store = create_test_store();
    let state = test_router_state(store); // ToolConfig::default()
    let app = create_router(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["run_command_enabled"], false,
        "aura-tools ToolConfig::default() is the kernel-level fail-closed baseline"
    );
    assert_eq!(json["shell_enabled"], false);
    assert!(
        json["binary_allowlist"].as_array().unwrap().is_empty(),
        "default ToolConfig should report an empty binary allowlist"
    );
}

#[tokio::test]
async fn test_submit_tx_valid() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let payload_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "Hello agent");

    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "user_prompt",
        "payload": payload_b64
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["accepted"].as_bool().unwrap());
    assert!(json["tx_id"].is_string());
}

#[tokio::test]
async fn test_submit_tx_invalid_agent_id() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let body = serde_json::json!({
        "agent_id": "not-hex",
        "kind": "user_prompt",
        "payload": "aGVsbG8="
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_submit_tx_invalid_kind() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "invalid_kind",
        "payload": "aGVsbG8="
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_submit_tx_invalid_base64() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "user_prompt",
        "payload": "!!! not base64 !!!"
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_submit_tx_rejects_mid_session_permissions_change() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let payload_json = serde_json::json!({
        "kind": "agent_permissions",
        "capabilities": [{"type": "spawnAgent"}]
    });
    let payload_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        serde_json::to_vec(&payload_json).unwrap(),
    );
    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "system",
        "payload": payload_b64,
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("permissions:"), "got: {text}");
    assert!(text.contains("frozen"), "got: {text}");
}

#[tokio::test]
async fn test_submit_tx_allows_normal_system_payload() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let payload = serde_json::json!({"kind": "identity", "name": "agent-x"});
    let payload_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        serde_json::to_vec(&payload).unwrap(),
    );
    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "system",
        "payload": payload_b64,
    });

    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_get_head_new_agent() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let req = authed_request()
        .uri(format!("/agents/{}/head", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["head_seq"], 0);
}

#[tokio::test]
async fn test_get_head_invalid_agent_id() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = authed_request()
        .uri("/agents/zzz-bad/head")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_scan_record_empty() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let req = authed_request()
        .uri(format!("/agents/{}/record", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_scan_record_with_query_params() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let req = authed_request()
        .uri(format!(
            "/agents/{}/record?from_seq=5&limit=10",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_scan_record_invalid_agent() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = authed_request()
        .uri("/agents/bad-hex/record")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_submit_tx_all_kinds() {
    let kinds = [
        "user_prompt",
        "agent_msg",
        "trigger",
        "action_result",
        "system",
    ];

    for kind in kinds {
        let store = create_test_store();
        let state = test_router_state(store);
        let app = create_router(state);

        let agent_id = AgentId::generate();
        let payload_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("payload for {kind}"),
        );

        let body = serde_json::json!({
            "agent_id": agent_id.to_hex(),
            "kind": kind,
            "payload": payload_b64
        });

        let req = authed_request()
            .method("POST")
            .uri("/tx")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "kind '{kind}' should be accepted"
        );
    }
}

#[tokio::test]
async fn test_nonexistent_route_returns_404() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = authed_request()
        .uri("/nonexistent")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// Helper: RouterState with real memory + skill managers
// ============================================================================

fn test_router_state_with_managers() -> RouterState {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.keep();
    let rocks = RocksStore::open(&dir, false).unwrap();
    let db = rocks.db_handle().clone();
    let store: Arc<dyn Store> = Arc::new(rocks);

    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("mock"));
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        std::path::PathBuf::from("/tmp/workspaces"),
        None,
    ));

    let memory_kernel = Arc::new(
        Kernel::new(
            store.clone(),
            provider.clone(),
            ExecutorRouter::new(),
            KernelConfig::default(),
            AgentId::generate(),
        )
        .unwrap(),
    );
    let memory_gateway = Arc::new(KernelModelGateway::new(memory_kernel));
    let memory_manager = Arc::new(MemoryManager::new(
        db.clone(),
        memory_gateway,
        RefinerConfig::default(),
        WriteConfig::default(),
        RetrievalConfig::default(),
        ConsolidationConfig::default(),
        ProcedureConfig::default(),
    ));

    let skill_store = Arc::new(SkillInstallStore::new(db.clone()));
    let loader = SkillLoader::with_defaults(None, None);
    let skill_manager = Arc::new(std::sync::RwLock::new(SkillManager::with_install_store(
        loader,
        skill_store,
    )));

    let secrets_vault = Arc::new(aura_store_db::SecretsVault::new(db.clone()));
    let process_store = Arc::new(aura_store_db::ProcessStore::new(db));

    // See note on `test_router_state` - opt in to bearer enforcement
    // because the ambient default is now off but the rest of this
    // suite is built around 401s and matching the test Bearer token.
    let config = NodeConfig {
        require_auth: true,
        ..NodeConfig::default()
    };
    RouterState::new(crate::gateway::RouterStateConfig {
        store,
        scheduler,
        config,
        provider,
        tool_config: ToolConfig::default(),
        catalog: Arc::new(ToolCatalog::new()),
        domain_api: None,
        automaton_controller: None,
        automaton_bridge: None,
        memory_manager: Some(memory_manager),
        skill_manager: Some(skill_manager),
        secrets_vault: Some(secrets_vault),
        process_store: Some(process_store),
        trigger_registrar: None,
        router_url: None,
    })
}

// ============================================================================
// Memory Facts
// ============================================================================

#[tokio::test]
async fn test_memory_create_and_list_facts() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({
        "key": "language",
        "value": "Rust",
        "confidence": 0.9,
        "importance": 0.7
    });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let facts: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0]["key"], "language");
}

#[tokio::test]
async fn test_memory_get_fact_by_key() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "key": "framework", "value": "Axum" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!(
            "/memory/{}/facts/by-key/framework",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let fact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fact["key"], "framework");
    assert_eq!(fact["value"], "Axum");
}

#[tokio::test]
async fn test_memory_delete_fact() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "key": "temp", "value": "delete me" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let fact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let fact_id = fact["fact_id"].as_str().unwrap();

    let req = authed_request()
        .method("DELETE")
        .uri(format!("/memory/{}/facts/{}", agent_id.to_hex(), fact_id))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let req = authed_request()
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let facts: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(facts.is_empty());
}

#[tokio::test]
async fn test_memory_api_enforces_project_agent_project_and_user_partitions() {
    let state = test_router_state_with_managers();
    let agent_a = AgentId::generate();
    let agent_b = AgentId::generate();
    let app = create_router(state);

    for (agent_id, key, scope) in [
        (agent_a, "agent_a_private", "agent"),
        (agent_a, "project_shared", "project"),
        (agent_a, "user_personal", "user"),
        (agent_b, "agent_b_private", "agent"),
    ] {
        let body = serde_json::json!({
            "key": key,
            "value": key,
            "continuity": {
                "scope": scope,
                "status": "active",
                "sensitivity": "normal",
                "pinned": false,
                "provenance": {}
            }
        });
        let request = authed_request()
            .method("POST")
            .uri(format!(
                "/memory/{}/facts?project_id=project-x&user_id=user-u",
                agent_id.to_hex()
            ))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }

    let legacy = serde_json::json!({ "key": "legacy_unscoped", "value": "legacy" });
    let request = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_b.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&legacy).unwrap()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );

    let request = authed_request()
        .uri(format!(
            "/memory/{}/snapshot?project_id=project-x&user_id=user-u",
            agent_b.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let packet: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let keys: std::collections::HashSet<_> = packet["facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|fact| fact["key"].as_str())
        .collect();
    assert_eq!(
        keys,
        std::collections::HashSet::from(["agent_b_private", "project_shared", "user_personal"])
    );

    let request = authed_request()
        .uri(format!(
            "/memory/{}/snapshot?project_id=project-y&user_id=user-u",
            agent_b.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let packet: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let keys: Vec<_> = packet["facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|fact| fact["key"].as_str())
        .collect();
    assert_eq!(keys, vec!["user_personal"]);

    let request = authed_request()
        .uri(format!(
            "/memory/{}/snapshot?project_id=project-x&user_id=user-u&include_legacy=true",
            agent_b.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let packet: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(packet["facts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|fact| fact["key"] == "legacy_unscoped"));

    let conflicting_personal = serde_json::json!({
        "key": "user_personal",
        "value": "contradictory preference",
        "continuity": {
            "scope": "user",
            "status": "active",
            "sensitivity": "normal",
            "pinned": false,
            "provenance": {}
        }
    });
    let request = authed_request()
        .method("POST")
        .uri(format!(
            "/memory/{}/facts?project_id=project-x&user_id=user-u",
            agent_b.to_hex()
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&conflicting_personal).unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let fact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fact["continuity"]["status"], "pending");
}

#[tokio::test]
async fn test_memory_continuity_config_roundtrip_and_empty_trace() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let req = authed_request()
        .uri(format!(
            "/api/agents/{}/memory/continuity",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let default_config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(default_config["use_memory"], true);
    assert_eq!(default_config["retrieval_mode"], "query_aware");

    let config = serde_json::json!({
        "use_memory": true,
        "generate_memory": true,
        "write_policy": "approval",
        "retrieval_mode": "query_aware",
        "allow_user_scope": false,
        "allow_project_scope": false
    });
    let req = authed_request()
        .method("PUT")
        .uri(format!(
            "/api/agents/{}/memory/continuity",
            agent_id.to_hex()
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&config).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!(
            "/api/agents/{}/memory/continuity",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let persisted: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(persisted["write_policy"], "approval");

    let req = authed_request()
        .uri(format!(
            "/api/agents/{}/memory/retrieval/latest",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn test_memory_retrieval_trace_is_scoped_to_project_agent_partition() {
    let state = test_router_state_with_managers();
    let manager = state.memory_manager.as_ref().unwrap().clone();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({
        "key": "release_train",
        "value": "Tuesday",
        "continuity": {
            "scope": "project",
            "status": "active",
            "sensitivity": "normal",
            "pinned": false,
            "provenance": {}
        }
    });
    let request = authed_request()
        .method("POST")
        .uri(format!(
            "/memory/{}/facts?project_id=project-x&user_id=user-u",
            agent_id.to_hex()
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );

    let mut prompt = String::new();
    manager
        .prepare_context_with_query(
            agent_id,
            &mut prompt,
            MemoryQueryContext {
                text: "When is the release train?".to_string(),
                access: MemoryAccessContext {
                    project_id: Some("project-x".to_string()),
                    user_id: Some("user-u".to_string()),
                    include_legacy: false,
                },
                ..MemoryQueryContext::default()
            },
        )
        .await;
    assert!(prompt.contains("Tuesday"));

    let request = authed_request()
        .uri(format!(
            "/api/agents/{}/memory/retrieval/latest?project_id=project-x&user_id=user-u",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let trace: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(trace["selected_count"], 1);

    let request = authed_request()
        .uri(format!(
            "/api/agents/{}/memory/retrieval/latest?project_id=project-y&user_id=user-u",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        serde_json::Value::Null
    );
}

// ============================================================================
// Memory Events
// ============================================================================

#[tokio::test]
async fn test_memory_create_and_list_events() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({
        "event_type": "task_run",
        "summary": "completed build"
    });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/events", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!("/memory/{}/events", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "task_run");
}

#[tokio::test]
async fn test_memory_bulk_delete_events_alias() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({
        "event_type": "task_run",
        "summary": "completed build"
    });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/events", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    // Phase B / Commit 3 follow-up: `delete_events_before` truncates
    // the cutoff to millisecond resolution before comparing with the
    // event key, so the test must guarantee `before > event.timestamp`
    // at that resolution. The pre-Phase-B run was lucky to fall on a
    // millisecond boundary every time; the engine extraction
    // re-ordered the test schedule enough that the cutoff and the
    // event sometimes landed in the same millisecond and the deletion
    // saw zero matches. A 5ms sleep is comfortably above Windows'
    // ~1ms timer resolution and adds nothing meaningful to the suite
    // wall clock.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let bulk_delete_body = serde_json::json!({
        "before": chrono::Utc::now().to_rfc3339()
    });
    let req = authed_request()
        .method("POST")
        .uri(format!(
            "/api/agents/{}/memory/events/bulk-delete",
            agent_id.to_hex()
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&bulk_delete_body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["deleted"], 1);

    let req = authed_request()
        .uri(format!("/memory/{}/events", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let events: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(events.is_empty());
}

// ============================================================================
// Memory Procedures
// ============================================================================

#[tokio::test]
async fn test_memory_create_and_list_procedures() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({
        "name": "deploy",
        "trigger": "user says deploy",
        "steps": ["cargo build", "cargo test", "deploy binary"]
    });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/procedures", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!("/memory/{}/procedures", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let procs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0]["name"], "deploy");
}

// ============================================================================
// Memory Stats & Wipe
// ============================================================================

#[tokio::test]
async fn test_memory_stats() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let req = authed_request()
        .uri(format!("/memory/{}/stats", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(stats["facts"], 0);
    assert_eq!(stats["events"], 0);
    assert_eq!(stats["procedures"], 0);
}

#[tokio::test]
async fn test_memory_wipe() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "key": "k", "value": "v" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/wipe", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let req = authed_request()
        .uri(format!("/memory/{}/stats", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(stats["facts"], 0);
    assert_eq!(stats["events"], 0);
}

#[tokio::test]
async fn test_memory_snapshot() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "key": "lang", "value": "Rust" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let _ = app.clone().oneshot(req).await.unwrap();

    let req = authed_request()
        .uri(format!("/memory/{}/snapshot", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot["facts"].as_array().unwrap().len(), 1);
    assert!(snapshot["events"].as_array().unwrap().is_empty());
    assert!(snapshot["procedures"].as_array().unwrap().is_empty());
}

// ============================================================================
// Memory — invalid agent id
// ============================================================================

#[tokio::test]
async fn test_memory_invalid_agent_id() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let req = authed_request()
        .uri("/memory/bad-hex/facts")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================================
// Memory — 503 when not configured
// ============================================================================

#[tokio::test]
async fn test_memory_returns_503_when_not_configured() {
    let store = create_test_store();
    let state = test_router_state(store);
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let req = authed_request()
        .uri(format!("/memory/{}/facts", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ============================================================================
// Skills
// ============================================================================

#[tokio::test]
async fn test_skills_list() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let req = authed_request()
        .uri("/api/skills")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let skills: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(skills.is_empty() || skills.iter().all(|s| s["name"].is_string()));
}

#[tokio::test]
async fn test_skills_get_not_found() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let req = authed_request()
        .uri("/api/skills/nonexistent")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_skills_agent_install_and_list() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "name": "test-skill" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/api/agents/{}/skills", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = authed_request()
        .uri(format!("/api/agents/{}/skills", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let installs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["skill_name"], "test-skill");
}

#[tokio::test]
async fn test_skills_agent_uninstall() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "name": "removable" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/api/agents/{}/skills", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = authed_request()
        .method("DELETE")
        .uri(format!(
            "/api/agents/{}/skills/removable",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let req = authed_request()
        .uri(format!("/api/agents/{}/skills", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let installs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(installs.is_empty());
}

#[tokio::test]
async fn test_skills_legacy_harness_aliases() {
    let state = test_router_state_with_managers();
    let agent_id = AgentId::generate();
    let app = create_router(state);

    let body = serde_json::json!({ "name": "legacy-skill" });
    let req = authed_request()
        .method("POST")
        .uri(format!("/api/harness/agents/{}/skills", agent_id.to_hex()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = authed_request()
        .uri(format!("/api/harness/agents/{}/skills", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let installs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["skill_name"], "legacy-skill");

    let req = authed_request()
        .method("DELETE")
        .uri(format!(
            "/api/harness/agents/{}/skills/legacy-skill",
            agent_id.to_hex()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let req = authed_request()
        .uri(format!("/api/agents/{}/skills", agent_id.to_hex()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let installs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(installs.is_empty());
}

#[tokio::test]
async fn test_skills_returns_503_when_not_configured() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = authed_request()
        .uri("/api/skills")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ============================================================================
// Auth middleware integration tests (security audit — phase 1)
// ============================================================================
//
// These tests exercise the router-wide `require_bearer_mw` middleware by
// issuing unauthenticated requests against every non-`/health` route and
// asserting that the response is `401 UNAUTHORIZED`. They also confirm
// that `/health` stays reachable without a token (liveness / readiness
// probes run anonymously).
//
// The matrix is hand-maintained rather than auto-discovered from the
// router because axum does not expose route introspection. When adding
// a new route to `create_router`, add a matching entry here.

/// Every protected route in the router, expressed as `(method, uri)`
/// pairs. Concrete path params (`:agent_id`, `:automaton_id`, `:name`,
/// `:id`, `:key`, `:tx_id`) are substituted with values that would
/// normally reach the handler body; because the middleware rejects the
/// request before any extractor runs, the specific values don't matter.
const PROTECTED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/files"),
    ("GET", "/api/read-file"),
    ("GET", "/workspace/resolve"),
    ("POST", "/tx"),
    ("GET", "/tx/status/deadbeef/abcd"),
    ("GET", "/agents/deadbeef/head"),
    ("GET", "/agents/deadbeef/record"),
    ("GET", "/ws/terminal"),
    ("GET", "/stream/test-run"),
    ("POST", "/v1/run"),
    ("GET", "/v1/run/list"),
    ("GET", "/v1/run/test-run/status"),
    ("POST", "/v1/run/test-run/pause"),
    ("POST", "/v1/run/test-run/stop"),
    // Memory canonical
    ("GET", "/memory/deadbeef/facts"),
    ("POST", "/memory/deadbeef/facts"),
    ("GET", "/memory/deadbeef/facts/some-id"),
    ("PUT", "/memory/deadbeef/facts/some-id"),
    ("DELETE", "/memory/deadbeef/facts/some-id"),
    ("GET", "/memory/deadbeef/facts/by-key/some-key"),
    ("GET", "/memory/deadbeef/events"),
    ("POST", "/memory/deadbeef/events"),
    ("DELETE", "/memory/deadbeef/events/some-id"),
    ("POST", "/memory/deadbeef/events/bulk-delete"),
    ("GET", "/memory/deadbeef/procedures"),
    ("POST", "/memory/deadbeef/procedures"),
    ("GET", "/memory/deadbeef/procedures/some-id"),
    ("PUT", "/memory/deadbeef/procedures/some-id"),
    ("DELETE", "/memory/deadbeef/procedures/some-id"),
    ("GET", "/memory/deadbeef/snapshot"),
    ("POST", "/memory/deadbeef/wipe"),
    ("GET", "/memory/deadbeef/stats"),
    ("GET", "/memory/deadbeef/continuity"),
    ("PUT", "/memory/deadbeef/continuity"),
    ("GET", "/memory/deadbeef/retrieval/latest"),
    ("POST", "/memory/deadbeef/consolidate"),
    // Memory aliases
    ("GET", "/api/agents/deadbeef/memory"),
    ("DELETE", "/api/agents/deadbeef/memory"),
    ("GET", "/api/agents/deadbeef/memory/facts"),
    ("POST", "/api/agents/deadbeef/memory/facts"),
    ("GET", "/api/agents/deadbeef/memory/facts/some-id"),
    ("PUT", "/api/agents/deadbeef/memory/facts/some-id"),
    ("DELETE", "/api/agents/deadbeef/memory/facts/some-id"),
    ("GET", "/api/agents/deadbeef/memory/facts/by-key/some-key"),
    ("GET", "/api/agents/deadbeef/memory/events"),
    ("POST", "/api/agents/deadbeef/memory/events"),
    ("DELETE", "/api/agents/deadbeef/memory/events/some-id"),
    ("POST", "/api/agents/deadbeef/memory/events/bulk-delete"),
    ("GET", "/api/agents/deadbeef/memory/procedures"),
    ("POST", "/api/agents/deadbeef/memory/procedures"),
    ("GET", "/api/agents/deadbeef/memory/procedures/some-id"),
    ("PUT", "/api/agents/deadbeef/memory/procedures/some-id"),
    ("DELETE", "/api/agents/deadbeef/memory/procedures/some-id"),
    ("GET", "/api/agents/deadbeef/memory/stats"),
    ("GET", "/api/agents/deadbeef/memory/continuity"),
    ("PUT", "/api/agents/deadbeef/memory/continuity"),
    ("GET", "/api/agents/deadbeef/memory/retrieval/latest"),
    ("POST", "/api/agents/deadbeef/memory/consolidate"),
    // Skills
    ("GET", "/api/skills"),
    ("POST", "/api/skills"),
    ("GET", "/api/skills/some-skill"),
    ("POST", "/api/skills/some-skill/activate"),
    ("GET", "/api/agents/deadbeef/skills"),
    ("POST", "/api/agents/deadbeef/skills"),
    ("DELETE", "/api/agents/deadbeef/skills/some-skill"),
    // Legacy harness aliases
    ("GET", "/api/harness/agents/deadbeef/skills"),
    ("POST", "/api/harness/agents/deadbeef/skills"),
    ("DELETE", "/api/harness/agents/deadbeef/skills/some-skill"),
];

#[tokio::test]
async fn test_requires_bearer_on_protected_routes() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    for (idx, (method, uri)) in PROTECTED_ROUTES.iter().enumerate() {
        // NOTE: deliberately does not go through `authed_request()`.
        // We want the *unauthenticated* code path.
        //
        // Each iteration gets a distinct synthetic peer address so the
        // phase-9 per-IP governor doesn't start returning 429 partway
        // through the matrix — we're testing the auth middleware, not
        // the rate limiter.
        let mut req = Request::builder()
            .method(*method)
            .uri(*uri)
            .body(Body::empty())
            .unwrap();
        inject_fake_peer(&mut req, idx);

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expected 401 for {method} {uri}, got {}",
            resp.status()
        );
    }
}

// ============================================================================
// Secrets vault (Swarm TEE phase 6)
// ============================================================================

/// Full CRUD roundtrip over the HTTP surface: PUT → GET (reveal) →
/// GET (default, redacted) → DELETE → 404.
#[tokio::test]
async fn test_secrets_crud_roundtrip() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    // Create.
    let body = serde_json::json!({ "value": "sk-live-12345", "description": "stripe key" });
    let req = authed_request()
        .method("PUT")
        .uri("/secrets/stripe-key")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["secret"]["name"], "stripe-key");
    assert_eq!(json["secret"]["description"], "stripe key");
    assert!(
        json["secret"].get("value").is_none(),
        "PUT response must not echo the value"
    );

    // GET with ?reveal=true returns the value (the in-VM read path).
    let req = authed_request()
        .uri("/secrets/stripe-key?reveal=true")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["secret"]["value"], "sk-live-12345");

    // GET without reveal returns metadata only.
    let req = authed_request()
        .uri("/secrets/stripe-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !raw.contains("sk-live-12345"),
        "non-reveal GET must not include the value: {raw}"
    );

    // DELETE, then the secret is gone.
    let req = authed_request()
        .method("DELETE")
        .uri("/secrets/stripe-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri("/secrets/stripe-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let req = authed_request()
        .method("DELETE")
        .uri("/secrets/stripe-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The list endpoint must never return values, only names + metadata.
#[tokio::test]
async fn test_secrets_list_never_returns_values() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    for (name, value) in [("alpha", "value-alpha-xyz"), ("beta", "value-beta-xyz")] {
        let body = serde_json::json!({ "value": value });
        let req = authed_request()
            .method("PUT")
            .uri(format!("/secrets/{name}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let req = authed_request()
        .uri("/secrets")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();

    let secrets = json["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 2);
    assert_eq!(secrets[0]["name"], "alpha");
    assert_eq!(secrets[1]["name"], "beta");
    assert!(
        !raw.contains("value-alpha-xyz") && !raw.contains("value-beta-xyz"),
        "list response must never contain secret values: {raw}"
    );
}

/// Secrets routes sit on the protected sub-router: no bearer → 401.
#[tokio::test]
async fn test_secrets_require_bearer() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    for (method, uri) in [
        ("GET", "/secrets"),
        ("GET", "/secrets/some-name"),
        ("DELETE", "/secrets/some-name"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require a bearer token"
        );
    }
}

/// Invalid names are rejected before touching the store.
#[tokio::test]
async fn test_secrets_invalid_name_rejected() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let body = serde_json::json!({ "value": "v" });
    let req = authed_request()
        .method("PUT")
        .uri("/secrets/bad%20name")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ============================================================================
// Processes / automations (Swarm TEE phase 7)
// ============================================================================

/// Full CRUD pass over /v1/processes, including enable/disable via PUT.
#[tokio::test]
async fn test_processes_crud_roundtrip() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    // Create.
    let body = serde_json::json!({
        "name": "nightly-report",
        "description": "summarize the day",
        "cron": "0 3 * * *",
        "prompt": "Summarize yesterday's commits.",
        "config": { "channel": "ops" }
    });
    let req = authed_request()
        .method("POST")
        .uri("/v1/processes")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = json["process"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["process"]["enabled"], true, "enabled defaults to true");
    assert!(
        json["process"]["next_run_at"].is_string(),
        "next_run_at computed on create"
    );

    // List includes the full definition (in-VM API).
    let req = authed_request()
        .uri("/v1/processes")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["processes"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["processes"][0]["prompt"],
        "Summarize yesterday's commits."
    );

    // Update: disable + new cron.
    let body = serde_json::json!({ "enabled": false, "cron": "*/5 * * * *" });
    let req = authed_request()
        .method("PUT")
        .uri(format!("/v1/processes/{id}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["process"]["enabled"], false);
    assert_eq!(json["process"]["cron"], "*/5 * * * *");
    // Prompt untouched by a partial update.
    assert_eq!(json["process"]["prompt"], "Summarize yesterday's commits.");

    // Get reflects the update.
    let req = authed_request()
        .uri(format!("/v1/processes/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Delete, then 404.
    let req = authed_request()
        .method("DELETE")
        .uri(format!("/v1/processes/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = authed_request()
        .uri(format!("/v1/processes/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Phase 8: a successful process mutation fires a best-effort push of
/// the trigger-metadata set to the swarm gateway's internal
/// replace-sync endpoint — with the internal bearer token, the
/// configured agent id, and a payload of metadata only (no prompt).
#[tokio::test]
async fn test_process_mutation_pushes_trigger_registration() {
    use crate::trigger_registrar::{RegistrarTarget, TriggerRegistrar};

    type Captured = (String, String, String);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Captured>(8);

    // Stub swarm gateway capturing (agent_id, authorization, body).
    let stub = Router::new()
        .route(
            "/internal/agents/:agent_id/process-triggers",
            axum::routing::put(
                |State(tx): State<tokio::sync::mpsc::Sender<Captured>>,
                 Path(agent_id): Path<String>,
                 headers: HeaderMap,
                 body: String| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let _ = tx.send((agent_id, auth, body)).await;
                    StatusCode::OK
                },
            ),
        )
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, stub).await.ok();
    });

    let mut state = test_router_state_with_managers();
    let process_store = state.process_store.clone().unwrap();
    state.trigger_registrar = Some(Arc::new(TriggerRegistrar::new(
        Some(RegistrarTarget {
            base_url: format!("http://{stub_addr}"),
            token: "internal-secret".into(),
            agent_id: "feedc0de".into(),
        }),
        process_store,
    )));
    let app = create_router(state);

    // Mutate: create a process with an in-TEE prompt + config.
    let body = serde_json::json!({
        "name": "nightly-report",
        "cron": "0 3 * * *",
        "prompt": "IN-TEE-PROMPT: summarize my sealed notes",
        "config": { "channel": "ops" }
    });
    let req = authed_request()
        .method("POST")
        .uri("/v1/processes")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The push happens on a background task — await its arrival.
    let (agent_id, auth, pushed) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("registration push must arrive after a mutation")
        .expect("stub channel closed");

    assert_eq!(agent_id, "feedc0de");
    assert_eq!(auth, "Bearer internal-secret");

    // The pushed body is the full desired set: metadata only.
    let payload: serde_json::Value = serde_json::from_str(&pushed).unwrap();
    let entries = payload.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["cron"], "0 3 * * *");
    assert_eq!(entries[0]["enabled"], true);
    assert!(entries[0]["process_id"].is_string());
    assert!(
        !pushed.contains("IN-TEE-PROMPT") && !pushed.contains("prompt"),
        "prompt must never cross the trust boundary; pushed: {pushed}"
    );
    assert!(!pushed.contains("ops") && !pushed.contains("config"));

    // Delete the process: the replace-sync now pushes an empty set.
    let req = authed_request()
        .uri("/v1/processes")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = json["processes"][0]["id"].as_str().unwrap().to_string();

    let req = authed_request()
        .method("DELETE")
        .uri(format!("/v1/processes/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_, _, pushed) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("delete must push the (now empty) desired set")
        .expect("stub channel closed");
    assert_eq!(pushed.trim(), "[]");
}

/// Invalid cron expressions are rejected with 400 on create and update.
#[tokio::test]
async fn test_processes_invalid_cron_rejected() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let body = serde_json::json!({
        "name": "broken",
        "cron": "not a cron",
        "prompt": "do things"
    });
    let req = authed_request()
        .method("POST")
        .uri("/v1/processes")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Create a valid one, then try to update it with a bad cron.
    let body = serde_json::json!({
        "name": "ok", "cron": "0 3 * * *", "prompt": "do things"
    });
    let req = authed_request()
        .method("POST")
        .uri("/v1/processes")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = json["process"]["id"].as_str().unwrap();

    let body = serde_json::json!({ "cron": "99 99 99 99 99" });
    let req = authed_request()
        .method("PUT")
        .uri(format!("/v1/processes/{id}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Process routes sit on the protected sub-router: no bearer → 401.
#[tokio::test]
async fn test_processes_require_bearer() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    for (method, uri) in [
        ("GET", "/v1/processes"),
        ("GET", "/v1/processes/some-id"),
        ("DELETE", "/v1/processes/some-id"),
        ("POST", "/v1/processes/some-id/trigger"),
        ("GET", "/v1/processes/some-id/runs"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require a bearer token"
        );
    }
}

/// `POST /v1/processes/:id/trigger` executes the process prompt
/// through the real chat-run path (MockProvider model, real driver +
/// kernel) and records a run that transitions Running → Success, with
/// `last_run_at` / `next_run_at` bookkeeping on the definition.
#[tokio::test]
async fn test_process_trigger_executes_run_and_records_history() {
    let (state, _tmp) = test_router_state_with_workspace();
    let app = create_router(state);

    // Create the process.
    let body = serde_json::json!({
        "name": "triggered",
        "cron": "0 4 * * *",
        "prompt": "Run the nightly checks."
    });
    let req = authed_request()
        .method("POST")
        .uri("/v1/processes")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = json["process"]["id"].as_str().unwrap().to_string();

    // Trigger: runs are async → 202 with the run record.
    let req = authed_request()
        .method("POST")
        .uri(format!("/v1/processes/{id}/trigger"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let run_id = json["run"]["run_id"].as_str().unwrap().to_string();
    assert_eq!(json["run"]["status"], "running");
    assert_eq!(
        json["event_stream_url"],
        format!("/stream/{run_id}"),
        "triggered runs are attachable through the standard stream route"
    );

    // Poll run history until the MockProvider turn completes.
    let mut last_status = String::new();
    for _ in 0..600 {
        let req = authed_request()
            .uri(format!("/v1/processes/{id}/runs"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let runs = json["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["run_id"], run_id.as_str());
        last_status = runs[0]["status"].as_str().unwrap_or_default().to_string();
        if last_status != "running" {
            assert_eq!(
                last_status, "success",
                "run must finish successfully (error: {:?})",
                runs[0]["error"]
            );
            assert!(runs[0]["finished_at"].is_string());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        last_status, "success",
        "timed out waiting for run completion"
    );

    // Definition bookkeeping: last_run_at stamped, next_run_at fresh.
    let req = authed_request()
        .uri(format!("/v1/processes/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["process"]["last_run_at"].is_string());
    assert!(json["process"]["next_run_at"].is_string());
}

/// Triggering an unknown process must 404 without spawning anything.
#[tokio::test]
async fn test_process_trigger_unknown_id_404() {
    let (state, _tmp) = test_router_state_with_workspace();
    let app = create_router(state);

    let req = authed_request()
        .method("POST")
        .uri("/v1/processes/00000000-0000-0000-0000-000000000000/trigger")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Overwrite the default `ConnectInfo<SocketAddr>` that
/// `router::ensure_connect_info` would otherwise inject, so callers
/// can appear to the governor as different peer IPs.
///
/// Used by tests that exercise a bearer-less loop over many routes;
/// without this the synthetic loopback default makes every request
/// share one rate-limit bucket and the loop trips 429 before it's
/// done.
fn inject_fake_peer(req: &mut Request<Body>, seed: usize) {
    use axum::extract::ConnectInfo;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    // Stay between 10.0.0.2 and 10.0.0.251 so we never pick .0, .1,
    // or .255. `u8::try_from` here can't actually fail because
    // `seed % 250 + 2 <= 251`, but clippy's `cast_possible_truncation`
    // lint fires on a plain `as u8`.
    let octet = u8::try_from(seed % 250 + 2).expect("octet fits in u8 by construction");
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, octet)), 0);
    req.extensions_mut().insert(ConnectInfo(addr));
}

#[tokio::test]
async fn test_rejects_malformed_bearer_header() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    // Wrong scheme — `Basic` instead of `Bearer`. The value after the
    // scheme is an arbitrary non-credential placeholder; the assertion
    // below is purely about the scheme, and a base64-shaped literal
    // here tripped GitHub secret scanning for no defensive benefit.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Basic placeholder")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct scheme but empty token.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer   ")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Phase 4 (security audit): a syntactically-valid Bearer that does
/// *not* match the configured secret must be rejected. This is the
/// regression test for the pre-phase-4 behaviour where `Bearer x`
/// was enough to reach any protected handler.
#[tokio::test]
async fn test_rejects_non_matching_bearer() {
    let state = test_router_state_with_managers();
    let app = create_router(state);

    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer not-the-configured-secret")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Swarm TEE R2 integration fix: when `AURA_SWARM_INTERNAL_TOKEN` is
/// configured (the scheduler injects it into confidential pods), the
/// gateway accepts it as a valid bearer on protected routes — that is
/// the token the swarm cron service presents on
/// `POST /v1/processes/:id/trigger`. The per-node token keeps working,
/// and arbitrary tokens are still rejected.
#[tokio::test]
async fn test_accepts_swarm_internal_token_on_protected_routes() {
    let mut state = test_router_state_with_managers();
    state.config.swarm_internal_token = Some("platform-internal-secret".to_string());
    let app = create_router(state);

    // The swarm internal token authenticates.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer platform-internal-secret")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The per-node token still authenticates alongside it.
    let req = authed_request()
        .uri("/api/skills")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A non-matching token is still rejected.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer some-other-token")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A server whose `auth_token` is empty (misconfiguration) must not
/// accept *any* request — otherwise attackers who guess that the
/// server "never loaded a secret" could send `Bearer ""` and win.
#[tokio::test]
async fn test_rejects_when_server_auth_token_empty() {
    let dir = tempfile::tempdir().unwrap().keep();
    let rocks = RocksStore::open(&dir, false).unwrap();
    let store: Arc<dyn Store> = Arc::new(rocks);

    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("mock"));
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        std::path::PathBuf::from("/tmp/workspaces"),
        None,
    ));
    let config = NodeConfig {
        auth_token: String::new(),
        require_auth: true,
        ..NodeConfig::default()
    };
    let state = RouterState::new(crate::gateway::RouterStateConfig {
        store,
        scheduler,
        config,
        provider,
        tool_config: ToolConfig::default(),
        catalog: Arc::new(ToolCatalog::new()),
        domain_api: None,
        automaton_controller: None,
        automaton_bridge: None,
        memory_manager: None,
        skill_manager: None,
        secrets_vault: None,
        process_store: None,
        trigger_registrar: None,
        router_url: None,
    });
    let app = create_router(state);

    // Even with the "correct" (empty) token the server must refuse.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer anything")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And of course with an empty presented token too.
    let req = Request::builder()
        .uri("/api/skills")
        .header("authorization", "Bearer ")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_health_remains_anonymous() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/health must remain reachable without a bearer token (liveness probe)"
    );
}

// ============================================================================
// File handler security tests (security audit — phase 3)
// ============================================================================
//
// These tests exercise `/api/read-file` and `/api/files` against a real
// on-disk workspace under `TempDir`, asserting that:
//   * canonicalized path traversal is rejected (403 Forbidden),
//   * legitimate in-workspace reads succeed with the file contents,
//   * oversize files trip the 5 MiB cap with 413 Payload Too Large.
//
// The previous `Path::starts_with` check on the unresolved input was
// bypassable with `../` sequences — these tests pin the behaviour of
// the canonicalizing `NodeConfig::resolve_allowed_path` replacement.

/// Build a router state whose workspace root is a real temp directory.
///
/// `data_dir/workspaces` is created so `resolve_allowed_path` can
/// canonicalize it. Returns the state and the `TempDir` so the caller
/// can keep it alive for the duration of the test.
fn test_router_state_with_workspace() -> (RouterState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("workspaces")).unwrap();

    let db_dir = tempfile::tempdir().unwrap().keep();
    let rocks = RocksStore::open(&db_dir, false).unwrap();
    let db = rocks.db_handle().clone();
    let store: Arc<dyn Store> = Arc::new(rocks);
    let process_store = Arc::new(aura_store_db::ProcessStore::new(db));

    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("mock"));
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        data_dir.join("workspaces"),
        None,
    ));
    let config = NodeConfig {
        data_dir,
        ..NodeConfig::default()
    };
    let state = RouterState::new(crate::gateway::RouterStateConfig {
        store,
        scheduler,
        config,
        provider,
        tool_config: ToolConfig::default(),
        catalog: Arc::new(ToolCatalog::new()),
        domain_api: None,
        automaton_controller: None,
        automaton_bridge: None,
        memory_manager: None,
        skill_manager: None,
        secrets_vault: None,
        process_store: Some(process_store),
        trigger_registrar: None,
        router_url: None,
    });
    (state, tmp)
}

#[tokio::test]
async fn test_read_file_rejects_path_traversal() {
    let (state, tmp) = test_router_state_with_workspace();
    let workspaces = tmp.path().join("workspaces");
    // Drop a secret file *outside* the workspace root to serve as the
    // target of the traversal attempt. The canonical resolver should
    // refuse to expose it even though it's a real, readable file on
    // disk under `data_dir/`.
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "top-secret").unwrap();
    // Also drop a decoy file inside `workspaces` so the traversal has
    // a valid starting segment to anchor against.
    std::fs::write(workspaces.join("decoy.txt"), "ok").unwrap();

    let app = create_router(state);

    // `workspaces/../secret.txt` canonicalises to `<tmp>/secret.txt`
    // which is one level above the root — must be rejected.
    let traversal = format!(
        "{}/../secret.txt",
        workspaces.to_string_lossy().replace('\\', "/")
    );
    let uri = format!("/api/read-file?path={}", urlencode(&traversal));
    let req = authed_request().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "traversal must be refused, not return the secret"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("top-secret"),
        "response must not leak file contents; got: {text}"
    );
}

#[tokio::test]
async fn test_read_file_returns_workspace_file() {
    let (state, tmp) = test_router_state_with_workspace();
    let workspaces = tmp.path().join("workspaces");
    let file_path = workspaces.join("hello.txt");
    std::fs::write(&file_path, "hello, world").unwrap();

    let app = create_router(state);

    let uri = format!(
        "/api/read-file?path={}",
        urlencode(&file_path.to_string_lossy())
    );
    let req = authed_request().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["content"], "hello, world");
}

#[tokio::test]
async fn test_read_file_caps_at_5mib() {
    let (state, tmp) = test_router_state_with_workspace();
    let workspaces = tmp.path().join("workspaces");
    // 5 MiB + 1 byte — one byte over the cap is the minimum signal
    // that the cap is enforced and not off-by-one in our favour.
    let oversize = workspaces.join("big.bin");
    let payload = vec![b'A'; 5 * 1024 * 1024 + 1];
    std::fs::write(&oversize, &payload).unwrap();

    let app = create_router(state);

    let uri = format!(
        "/api/read-file?path={}",
        urlencode(&oversize.to_string_lossy())
    );
    let req = authed_request().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "oversize reads must trip the 5 MiB cap, not OOM the process"
    );
}

#[tokio::test]
async fn test_list_files_rejects_path_traversal() {
    let (state, tmp) = test_router_state_with_workspace();
    let workspaces = tmp.path().join("workspaces");
    std::fs::write(workspaces.join("inside.txt"), "ok").unwrap();

    let app = create_router(state);

    let traversal = format!("{}/../..", workspaces.to_string_lossy().replace('\\', "/"));
    let uri = format!("/api/files?path={}", urlencode(&traversal));
    let req = authed_request().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "listing above the workspace root must be refused"
    );
}

// ============================================================================
// Phase 9 (security audit): DoS-protection tests — body limits, rate
// limiting, concurrency caps.
// ============================================================================

/// `POST /tx` with a body larger than the 1 MiB global cap must be
/// refused at the layer level before the handler runs. axum surfaces
/// `DefaultBodyLimit` violations as `413 Payload Too Large`.
#[tokio::test]
async fn test_tx_rejects_body_over_one_mib() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    // 1 MiB + 1 byte of arbitrary payload. The body doesn't have to
    // be valid JSON — the body-limit layer trips before serde runs.
    let oversize = vec![b'x'; 1024 * 1024 + 1];
    let req = authed_request()
        .method("POST")
        .uri("/tx")
        .header("content-type", "application/json")
        .body(Body::from(oversize))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "/tx bodies over 1 MiB must be rejected with 413 before reaching the handler"
    );
}

/// Flooding `/tx` from a single synthetic peer must trigger the
/// strict 5/sec burst-10 governor layer, producing at least one
/// `429 Too Many Requests` response within 100 rapid requests.
#[tokio::test]
async fn test_tx_rate_limit_returns_429_under_flood() {
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let agent_id = AgentId::generate();
    let payload_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "rate-limit");
    let body = serde_json::json!({
        "agent_id": agent_id.to_hex(),
        "kind": "user_prompt",
        "payload": payload_b64,
    });
    let body_bytes = serde_json::to_vec(&body).unwrap();

    let mut saw_429 = false;
    for _ in 0..100 {
        // Fixed peer — all 100 requests share one rate-limit bucket.
        let mut req = authed_request()
            .method("POST")
            .uri("/tx")
            .header("content-type", "application/json")
            .body(Body::from(body_bytes.clone()))
            .unwrap();
        inject_fake_peer(&mut req, 42);

        let resp = app.clone().oneshot(req).await.unwrap();
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }
    assert!(
        saw_429,
        "100 rapid /tx requests from one peer must trip the 5/sec burst-10 governor"
    );
}

/// Pure-unit test for the WS slot helper: a semaphore of size N lets
/// through N concurrent acquires; the (N+1)th fails. The production
/// handlers short-circuit to `503` on the `None` return.
#[test]
fn test_ws_slot_semaphore_rejects_over_capacity() {
    use super::handlers::run_ws::try_acquire_ws_slot;
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(3));

    let p1 = try_acquire_ws_slot(&sem).expect("first acquire should succeed");
    let p2 = try_acquire_ws_slot(&sem).expect("second acquire should succeed");
    let p3 = try_acquire_ws_slot(&sem).expect("third acquire should succeed");

    assert!(
        try_acquire_ws_slot(&sem).is_none(),
        "fourth acquire past capacity must return None (handler returns 503)"
    );

    // Releasing one permit makes the slot available again.
    drop(p1);
    let p4 = try_acquire_ws_slot(&sem).expect("slot must free after drop");

    drop(p2);
    drop(p3);
    drop(p4);
    assert_eq!(sem.available_permits(), 3);
}

/// Shared buffer the global tracing subscriber writes events into,
/// gated by [`CAPTURE_THREAD_ID`]: writes only land in the buffer when
/// the emitting thread matches the capturing test's thread, so events
/// from other tests running in parallel can't pollute the snapshot.
static CONSOLE_CAPTURE_BUF: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
static CONSOLE_CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CONSOLE_CAPTURE_INIT: std::sync::Once = std::sync::Once::new();
/// Thread ID of the test currently holding [`CONSOLE_CAPTURE_LOCK`].
/// `0` means "no test is capturing right now" — the writer drops the
/// event in that state, so we don't accidentally accumulate events
/// from unrelated parts of the test suite.
static CAPTURE_THREAD_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn current_thread_id_u64() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

struct CaptureWriter;
impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let target = CAPTURE_THREAD_ID.load(std::sync::atomic::Ordering::Acquire);
        if target != 0 && target == current_thread_id_u64() {
            CONSOLE_CAPTURE_BUF.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Lock-guarded handle on the shared capture buffer. Constructed via
/// [`ConsoleCapture::install`]; drop to release the lock.
struct ConsoleCapture {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ConsoleCapture {
    /// Install — exactly once per process — a global tracing subscriber
    /// that writes every event into [`CONSOLE_CAPTURE_BUF`], acquire
    /// the process-wide capture lock, and reset the buffer. Per-test
    /// thread-local subscribers don't survive `tokio` task hops or
    /// `tracing`'s callsite-interest cache when other tests run in
    /// parallel — owning the dispatcher process-wide and serializing
    /// capture sidesteps that race.
    fn install() -> Self {
        use tracing_subscriber::{fmt::layer, layer::SubscriberExt, util::SubscriberInitExt};

        CONSOLE_CAPTURE_INIT.call_once(|| {
            let subscriber = tracing_subscriber::registry().with(
                layer()
                    .with_ansi(false)
                    .with_writer(|| CaptureWriter)
                    .with_target(true),
            );
            let _ = subscriber.try_init();
        });

        let _lock = CONSOLE_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        CONSOLE_CAPTURE_BUF.lock().unwrap().clear();
        CAPTURE_THREAD_ID.store(
            current_thread_id_u64(),
            std::sync::atomic::Ordering::Release,
        );
        colored::control::set_override(false);
        Self { _lock }
    }

    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&CONSOLE_CAPTURE_BUF.lock().unwrap()).to_string()
    }
}

impl Drop for ConsoleCapture {
    fn drop(&mut self) {
        CAPTURE_THREAD_ID.store(0, std::sync::atomic::Ordering::Release);
        colored::control::unset_override();
    }
}

/// Phase: the inbound failure observer middleware must surface
/// auth-middleware 401s through the visual `aura::console` transcript
/// the same way the outbound LLM call surfaces a 403 Cloudflare block.
/// Failure to do so means an operator scanning a single log file
/// won't see "request was rejected by the harness" — they'd have to
/// hunt through the structured `tower_http::trace` rows. Pair this
/// with `cloudflare_block_round_trip_emits_paired_request_and_failure_blocks`
/// in `aura-reasoner` to keep both halves of the contract honest.
#[tokio::test]
async fn unauthorized_inbound_request_emits_paired_inbound_blocks() {
    let capture = ConsoleCapture::install();
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    let mut req = Request::builder()
        .method("POST")
        .uri("/tx")
        .body(Body::empty())
        .unwrap();
    inject_fake_peer(&mut req, 0);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing bearer token should produce a 401 through the auth middleware"
    );

    let captured = capture.snapshot();
    assert!(
        captured.contains("→ POST /tx"),
        "expected paired inbound request block, got transcript:\n{captured}"
    );
    assert!(
        captured.contains("← 401 unauthorized"),
        "expected paired inbound failure block with 401 header + reason label, got transcript:\n{captured}"
    );
}

/// Health probes must NOT show up in the visual transcript, even
/// when they 4xx — kubelet / load-balancer probes hit `/health` on
/// a fixed cadence and would otherwise drown out real rejections.
/// Pair-tests `unauthorized_inbound_request_emits_paired_inbound_blocks`
/// (`/tx` → emits) against `/health` (→ silent) to pin the skip list.
#[tokio::test]
async fn inbound_failure_observer_skips_health_probes() {
    let capture = ConsoleCapture::install();
    let store = create_test_store();
    let state = test_router_state(store);
    let app = create_router(state);

    // `/health` is a public route, so a GET succeeds — but the
    // observer must stay quiet either way. Using GET keeps this test
    // resilient to public-route changes; the assertion is "no inbound
    // block was rendered for this path".
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let captured = capture.snapshot();
    // The shared global subscriber captures events from every test
    // running in parallel — we only care that OUR observer didn't emit
    // its specific paired blocks for the health probe. Other concurrent
    // tests' debug noise is fine.
    assert!(
        !captured.contains("→ GET /health"),
        "/health probes must be excluded from the inbound observer's transcript, got:\n{captured}"
    );
    assert!(
        !captured.contains("← 404") && !captured.contains("← 405"),
        "inbound failure observer must not emit any failure block for /health, got:\n{captured}"
    );
}

/// Tiny percent-encoder used only by the file-handler tests.
///
/// We can't drag in a full URL crate just for this: `reqwest::Url`
/// isn't available in this test binary and `percent_encoding` isn't a
/// workspace dep. So we hand-encode the bytes that matter for URL
/// paths (space, `?`, `#`, `&`, `%`, `+`, non-ASCII) and leave the
/// alphanumerics / safe punctuation alone.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':' | b'\\');
        if safe {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}
