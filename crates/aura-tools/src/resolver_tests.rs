use super::*;
use aura_core_types::{
    ActionId, AgentId, InstalledToolDefinition, InstalledToolRuntimeAuth,
    InstalledToolRuntimeExecution, InstalledToolRuntimeIntegration,
    InstalledToolRuntimeProviderExecution, ToolAuth,
};
use aura_exec_traits::ExecuteContext;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use tempfile::TempDir;

const TRUSTED_INTEGRATION_RUNTIME_METADATA_KEY: &str = "trusted_integration_runtime";

fn make_catalog_and_resolver() -> (Arc<ToolCatalog>, ToolResolver) {
    let cat = Arc::new(ToolCatalog::new());
    let resolver = ToolResolver::new(cat.clone(), ToolConfig::default());
    (cat, resolver)
}

fn test_context() -> (ExecuteContext, TempDir) {
    let dir = TempDir::new().unwrap();
    let ctx = ExecuteContext::new(
        AgentId::generate(),
        ActionId::generate(),
        dir.path().to_path_buf(),
    );
    (ctx, dir)
}

fn spawn_single_response_server(
    expected_auth: Option<&str>,
    response_status: &str,
    response_body: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_auth = expected_auth.map(ToOwned::to_owned);
    let response_status = response_status.to_string();
    let response_body = response_body.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0_u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with("POST ") || request.starts_with("GET "),
            "request was: {request}"
        );
        assert!(
            request.contains("content-type: application/json")
                || request.contains("Content-Type: application/json")
        );
        if let Some(expected) = expected_auth {
            assert!(
                request.contains(&format!("Authorization: {expected}"))
                    || request.contains(&format!("authorization: {expected}")),
                "missing auth header in request: {request}"
            );
        }
        let response = format!(
            "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    format!("http://{addr}")
}

fn trusted_runtime_metadata(spec: serde_json::Value) -> std::collections::HashMap<String, Value> {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(TRUSTED_INTEGRATION_RUNTIME_METADATA_KEY.to_string(), spec);
    metadata
}

fn spawn_asserting_response_server(
    expected_method: &str,
    expected_headers: &[(&str, &str)],
    response_status: &str,
    response_body: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_method = expected_method.to_string();
    let expected_headers = expected_headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect::<Vec<_>>();
    let response_status = response_status.to_string();
    let response_body = response_body.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0_u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with(&format!("{expected_method} ")),
            "request was: {request}"
        );
        for (name, value) in expected_headers {
            assert!(
                request.contains(&format!("{name}: {value}"))
                    || request.contains(&format!("{}: {}", name.to_ascii_lowercase(), value)),
                "missing header `{name}: {value}` in request: {request}"
            );
        }
        let response = format!(
            "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    format!("http://{addr}")
}

fn spawn_asserting_request_server(
    expected_method: &str,
    expected_request_parts: &[&str],
    response_status: &str,
    response_body: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_method = expected_method.to_string();
    let expected_request_parts = expected_request_parts
        .iter()
        .map(|part| (*part).to_string())
        .collect::<Vec<_>>();
    let response_status = response_status.to_string();
    let response_body = response_body.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0_u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with(&format!("{expected_method} ")),
            "request was: {request}"
        );
        for expected in expected_request_parts {
            assert!(
                request.contains(&expected),
                "missing request part `{expected}` in request: {request}"
            );
        }
        let response = format!(
            "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    format!("http://{addr}")
}

#[test]
fn resolver_has_builtin_tools() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let tools = resolver.visible_tools(ToolProfile::Core);
    let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains("read_file"));
}

#[test]
fn visible_tools_returns_core() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let tools = resolver.visible_tools(ToolProfile::Core);
    let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains("read_file"));
}

#[tokio::test]
async fn execute_builtin_tool() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let (ctx, dir) = test_context();
    std::fs::write(dir.path().join("hello.txt"), "world").unwrap();

    let tc = ToolCall::fs_ls(".");
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
}

#[tokio::test]
async fn unknown_tool_returns_failed_effect() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("no_such_tool", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Failed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let err_msg = std::str::from_utf8(&result.stderr).unwrap();
    assert!(
        err_msg.contains("unknown tool"),
        "truly unknown tool should say 'unknown tool', got: {err_msg}",
    );
}

#[tokio::test]
async fn domain_tool_without_executor_falls_through_to_unknown() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("create_spec", serde_json::json!({"project_id": "p1"}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Failed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let err_msg = std::str::from_utf8(&result.stderr).unwrap();
    assert!(
        err_msg.contains("unknown tool"),
        "domain tool without executor should now be 'unknown tool', got: {err_msg}",
    );
}

#[tokio::test]
async fn installed_tool_executes_via_http_callback() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_single_response_server(
        Some("Bearer test-token"),
        "200 OK",
        r#"{"ok":true,"title":"Aura OS"}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "brave_search_web".into(),
        description: "Search the web".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint,
        auth: ToolAuth::Bearer {
            token: "test-token".into(),
        },
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: None,
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new(
        "brave_search_web",
        serde_json::json!({"query":"aura os","count":1}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "installed tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Aura OS\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_http_failure_returns_failed_effect() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_single_response_server(None, "404 Not Found", r#"{"error":"missing"}"#);
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "brave_search_web".into(),
        description: "Search the web".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint,
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: None,
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("brave_search_web", serde_json::json!({"query":"aura os"}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Failed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stderr = std::str::from_utf8(&result.stderr).unwrap();
    assert!(
        stderr.contains("returned status 404"),
        "stderr was: {stderr}"
    );
}

#[tokio::test]
async fn installed_tool_executes_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_single_response_server(
        Some("Bearer gh-test"),
        "200 OK",
        r#"[{"name":"aura","full_name":"cypher-asi/aura","private":false,"html_url":"https://github.com/cypher-asi/aura","default_branch":"main","description":"Aura"}]"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "github_list_repos".into(),
        description: "List GitHub repos".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "github".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "github-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "gh-test".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("github_list_repos", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "runtime-installed tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(
        stdout.contains("\"cypher-asi/aura\""),
        "stdout was: {stdout}"
    );
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_rest_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "POST",
        &[("Authorization", "Bearer gh-test")],
        "200 OK",
        r#"{"number":42,"title":"Ship it","state":"open","html_url":"https://github.com/cypher-asi/aura/issues/42"}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "github_create_issue".into(),
        description: "Create a GitHub issue".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "github".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "github-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "gh-test".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_json",
            "method":"post",
            "path":"/repos/{owner}/{repo}/issues",
            "query":[],
            "body":[
                {"argNames":["title"],"target":"title","valueType":"string","required":true},
                {"argNames":["body"],"target":"body","valueType":"string","required":false}
            ],
            "successGuard":"none",
            "result":{
                "type":"project_object",
                "key":"issue",
                "fields":[
                    {"output":"number","pointer":"/number"},
                    {"output":"title","pointer":"/title"},
                    {"output":"state","pointer":"/state"},
                    {"output":"html_url","pointer":"/html_url"}
                ]
            }
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "github_create_issue",
        serde_json::json!({"owner":"cypher-asi","repo":"aura","title":"Ship it","body":"hello"}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Ship it\""), "stdout was: {stdout}");
    assert!(stdout.contains("\"number\":42"), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_trusted_runtime_metadata_supports_boolean_query_args() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "GET",
        &[
            "GET /calendar/v3/calendars/primary/events?maxResults=2&singleEvents=true ",
            "authorization: Bearer google-secret",
        ],
        "200 OK",
        r#"{"items":[{"id":"evt_1","summary":"Planning","status":"confirmed"}]}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "google_calendar_list_events".into(),
        description: "List Google Calendar events".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "google".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "google-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "google-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_json",
            "method":"get",
            "path":"/calendar/v3/calendars/primary/events",
            "query":[
                {"argNames":["max_results"],"target":"maxResults","valueType":"positive_number","required":false},
                {"argNames":["single_events"],"target":"singleEvents","valueType":"boolean","required":false}
            ],
            "body":[],
            "successGuard":"none",
            "result":{"type":"wrap_pointer","key":"events","pointer":"/items"}
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "google_calendar_list_events",
        serde_json::json!({"max_results":2,"single_events":true}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Planning\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_graphql_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "POST",
        &[("Authorization", "lin-secret")],
        "200 OK",
        r#"{"data":{"teams":{"nodes":[{"id":"t1","name":"Zero","key":"ZER"}]}}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "linear_list_teams".into(),
        description: "List Linear teams".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "linear".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "linear-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationRaw {
                        value: "lin-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"graphql",
            "query":"query AuraLinearTeams { teams { nodes { id name key } } }",
            "variables":[],
            "successGuard":"graphql_errors",
            "result":{"type":"wrap_pointer","key":"teams","pointer":"/data/teams/nodes"}
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new("linear_list_teams", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"ZER\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_brave_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "GET",
        &[("X-Subscription-Token", "brave-secret")],
        "200 OK",
        r#"{"web":{"results":[{"title":"Aura","url":"https://zero.tech","description":"desc"}]},"query":{"more_results_available":false}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "brave_search_web".into(),
        description: "Search the web".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "brave_search".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "brave-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::Header {
                        name: "X-Subscription-Token".into(),
                        value: "brave-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"brave_search",
            "vertical":"web"
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "brave_search_web",
        serde_json::json!({"query":"aura","count":1}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Aura\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_resend_send_email() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "POST",
        &[("Authorization", "Bearer resend-secret")],
        "200 OK",
        r#"{"id":"email_123"}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "resend_send_email".into(),
        description: "Send email".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "resend".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "resend-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "resend-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"resend_send_email"
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "resend_send_email",
        serde_json::json!({"from":"Aura <onboarding@resend.dev>","to":"shahroz@wilderworld.com","subject":"Hello","text":"hello"}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"email_123\""), "stdout was: {stdout}");
}

#[test]
fn trusted_runtime_metadata_accepts_provider_specific_variants() {
    for spec in [
        serde_json::json!({"type":"brave_search","vertical":"web"}),
        serde_json::json!({"type":"resend_send_email"}),
        serde_json::json!({"type":"gmail_send_email"}),
        serde_json::json!({"type":"gmail_create_draft"}),
        serde_json::json!({"type":"google_calendar_create_event"}),
        serde_json::json!({"type":"google_calendar_update_event"}),
        serde_json::json!({"type":"google_calendar_delete_event"}),
    ] {
        let variant = spec["type"].as_str().unwrap().to_string();
        let tool = InstalledToolDefinition {
            name: variant.clone(),
            description: "Provider-specific trusted runtime".into(),
            input_schema: serde_json::json!({"type":"object"}),
            endpoint: "http://unused.local".into(),
            auth: ToolAuth::None,
            timeout_ms: Some(5_000),
            namespace: Some("aura_org_tools".into()),
            required_integration: None,
            runtime_execution: None,
            metadata: trusted_runtime_metadata(spec),
        };
        assert!(
            super::trusted::trusted_runtime_spec(&tool)
                .unwrap()
                .is_some(),
            "expected trusted runtime metadata variant `{variant}` to parse"
        );
    }
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_gmail_send_email() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "POST",
        &[
            "POST /gmail/v1/users/me/messages/send ",
            "authorization: Bearer google-access-token",
            "\"raw\":\"",
            "\"threadId\":\"thread-1\"",
        ],
        "200 OK",
        r#"{"id":"msg-123","threadId":"thread-1","labelIds":["SENT"]}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "gmail_send_email".into(),
        description: "Send Gmail".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "google".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "google-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "google-access-token".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"gmail_send_email"
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "gmail_send_email",
        serde_json::json!({
            "from":"shahroz@wilderworld.com",
            "to":"n30@wilderworld.com",
            "subject":"Google integration working smoothly",
            "text":"hello",
            "thread_id":"thread-1"
        }),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime Gmail send should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"msg-123\""), "stdout was: {stdout}");
    assert!(stdout.contains("\"SENT\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_google_calendar_create_event() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "POST",
        &[
            "POST /calendar/v3/calendars/primary/events?sendUpdates=all&conferenceDataVersion=1 ",
            "authorization: Bearer google-access-token",
            "\"summary\":\"Planning\"",
            "\"dateTime\":\"2026-06-08T10:00:00-04:00\"",
            "\"conferenceData\"",
        ],
        "200 OK",
        r#"{"id":"event-123","summary":"Planning","htmlLink":"https://calendar.google.com/event","status":"confirmed","start":{"dateTime":"2026-06-08T10:00:00-04:00"},"end":{"dateTime":"2026-06-08T10:30:00-04:00"},"attendees":[{"email":"n30@wilderworld.com"}],"conferenceData":{"conferenceId":"meet-123"}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "google_calendar_create_event".into(),
        description: "Create Google Calendar event".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "google".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "google-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "google-access-token".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"google_calendar_create_event"
        })),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "google_calendar_create_event",
        serde_json::json!({
            "calendar_id":"primary",
            "summary":"Planning",
            "start":"2026-06-08T10:00:00-04:00",
            "end":"2026-06-08T10:30:00-04:00",
            "attendees":["n30@wilderworld.com"],
            "send_updates":"all",
            "create_google_meet":true
        }),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime Calendar create should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"event-123\""), "stdout was: {stdout}");
    assert!(
        stdout.contains("\"conference_data\""),
        "stdout was: {stdout}"
    );
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_buffer_query_and_form_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "POST",
        &[
            "POST /updates/create.json?access_token=buffer-secret ",
            "content-type: application/x-www-form-urlencoded",
            "text=Ship+it",
            "profile_ids%5B%5D=profile-1",
        ],
        "200 OK",
        r#"{"success":true,"updates":[{"id":"update-1","status":"buffer","text":"Ship it","service":"twitter"}]}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "buffer_create_update".into(),
        description: "Create a Buffer update".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "buffer".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "buffer-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::QueryParam {
                        name: "access_token".into(),
                        value: "buffer-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_form",
            "method":"post",
            "path":"/updates/create.json",
            "body":[
                {"argNames":["text"],"target":"text","valueType":"string","required":true},
                {"argNames":["profile_id","profileId"],"target":"profile_ids[]","valueType":"string","required":true}
            ],
            "result":{
                "type":"project_array",
                "key":"updates",
                "pointer":"/updates",
                "fields":[
                    {"output":"id","pointer":"/id"},
                    {"output":"status","pointer":"/status"},
                    {"output":"text","pointer":"/text"},
                    {"output":"service","pointer":"/service"}
                ],
                "extras":[
                    {"output":"success","pointer":"/success","defaultValue":false}
                ]
            }
        })),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new(
        "buffer_create_update",
        serde_json::json!({"profile_id":"profile-1","text":"Ship it"}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime buffer tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"update-1\""), "stdout was: {stdout}");
    assert!(stdout.contains("\"success\":true"), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_metricool_provider_config_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "GET",
        &[
            "GET /admin/simpleProfiles?userId=user-123&blogId=blog-456 ",
            "x-mc-auth: metricool-secret",
        ],
        "200 OK",
        r#"[{"id":654321,"userId":123456,"label":"Aura Brand"}]"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "metricool_list_brands".into(),
        description: "List Metricool brands".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "metricool".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "metricool-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::Header {
                        name: "X-Mc-Auth".into(),
                        value: "metricool-secret".into(),
                    },
                    provider_config: std::collections::HashMap::from([
                        ("userId".into(), serde_json::json!("user-123")),
                        ("blogId".into(), serde_json::json!("blog-456")),
                    ]),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_json",
            "method":"get",
            "path":"/admin/simpleProfiles",
            "query":[
                {"argNames":["userId"],"target":"userId","source":"provider_config","valueType":"string","required":true},
                {"argNames":["blogId"],"target":"blogId","source":"provider_config","valueType":"string","required":true}
            ],
            "result":{
                "type":"project_array",
                "key":"brands",
                "fields":[
                    {"output":"id","pointer":"/id"},
                    {"output":"user_id","pointer":"/userId"},
                    {"output":"label","pointer":"/label"}
                ]
            }
        })),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("metricool_list_brands", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime metricool tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Aura Brand\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_apify_root_json_body() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "POST",
        &[
            "POST /acts/my-actor/runs ",
            "authorization: Bearer apify-secret",
            "{\"query\":\"aura\"}",
        ],
        "200 OK",
        r#"{"data":{"id":"run-1","status":"READY","actId":"actor-1"}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "apify_run_actor".into(),
        description: "Run an Apify actor".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "apify".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "apify-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "apify-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_json",
            "method":"post",
            "path":"/acts/{actor_id}/runs",
            "body":[
                {"argNames":["input"],"target":"$","valueType":"json","defaultValue":{}}
            ],
            "result":{
                "type":"project_object",
                "key":"run",
                "pointer":"/data",
                "fields":[
                    {"output":"id","pointer":"/id"},
                    {"output":"status","pointer":"/status"},
                    {"output":"act_id","pointer":"/actId"}
                ]
            }
        })),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new(
        "apify_run_actor",
        serde_json::json!({"actor_id":"my-actor","input":{"query":"aura"}}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime apify tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"run-1\""), "stdout was: {stdout}");
    assert!(stdout.contains("\"READY\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_via_trusted_runtime_metadata_mailchimp_base_url_override() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_request_server(
        "GET",
        &["GET /lists "],
        "200 OK",
        r#"{"lists":[{"id":"list-1","name":"Players","stats":{"member_count":128}}]}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "mailchimp_list_audiences".into(),
        description: "List Mailchimp audiences".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "mailchimp".into(),
                base_url: String::new(),
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "mailchimp-default".into(),
                    base_url: Some(endpoint),
                    auth: InstalledToolRuntimeAuth::Basic {
                        username: "anystring".into(),
                        password: "mailchimp-secret-us19".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: trusted_runtime_metadata(serde_json::json!({
            "type":"rest_json",
            "method":"get",
            "path":"/lists",
            "result":{
                "type":"project_array",
                "key":"audiences",
                "pointer":"/lists",
                "fields":[
                    {"output":"id","pointer":"/id"},
                    {"output":"name","pointer":"/name"},
                    {"output":"member_count","pointer":"/stats/member_count"}
                ]
            }
        })),
    }]);
    let (ctx, _dir) = test_context();

    let tc = ToolCall::new("mailchimp_list_audiences", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    assert!(result.ok, "trusted runtime mailchimp tool should succeed");
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"list-1\""), "stdout was: {stdout}");
    assert!(stdout.contains("\"Players\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_linear_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "POST",
        &[("Authorization", "lin-secret")],
        "200 OK",
        r#"{"data":{"teams":{"nodes":[{"id":"t1","name":"Zero","key":"ZER"}]}}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "linear_list_teams".into(),
        description: "List Linear teams".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "linear".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "linear-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationRaw {
                        value: "lin-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new("linear_list_teams", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"ZER\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_brave_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "GET",
        &[("X-Subscription-Token", "brave-secret")],
        "200 OK",
        r#"{"web":{"results":[{"title":"Aura","url":"https://zero.tech","description":"desc"}]},"query":{"more_results_available":false}}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "brave_search_web".into(),
        description: "Search the web".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "brave_search".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "brave-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::Header {
                        name: "X-Subscription-Token".into(),
                        value: "brave-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "brave_search_web",
        serde_json::json!({"query":"aura","count":1}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"Aura\""), "stdout was: {stdout}");
}

#[tokio::test]
async fn installed_tool_executes_slack_post_message_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "POST",
        &[("Authorization", "Bearer slack-secret")],
        "200 OK",
        r#"{"ok":true,"channel":"C0AR7PWBX4P","ts":"1775678730.921229"}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "slack_post_message".into(),
        description: "Post to Slack".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "slack".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "slack-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "slack-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new(
        "slack_post_message",
        serde_json::json!({"channel_id":"C0AR7PWBX4P","text":"hello from aura"}),
    );
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(
        stdout.contains("\"1775678730.921229\""),
        "stdout was: {stdout}"
    );
}

#[tokio::test]
async fn installed_tool_surfaces_slack_api_errors_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "GET",
        &[("Authorization", "Bearer slack-secret")],
        "200 OK",
        r#"{"ok":false,"error":"missing_scope"}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "slack_list_channels".into(),
        description: "List Slack channels".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "slack".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "slack-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "slack-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new("slack_list_channels", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Failed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stderr = std::str::from_utf8(&result.stderr).unwrap();
    assert!(
        stderr.contains("slack api error: missing_scope"),
        "stderr was: {stderr}"
    );
}

#[tokio::test]
async fn installed_tool_executes_resend_via_runtime_provider_path() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let endpoint = spawn_asserting_response_server(
        "GET",
        &[("Authorization", "Bearer resend-secret")],
        "200 OK",
        r#"{"data":[{"id":"dom_1","name":"example.com","status":"verified"}],"has_more":false}"#,
    );
    let resolver = resolver.with_installed_tools(vec![InstalledToolDefinition {
        name: "resend_list_domains".into(),
        description: "List resend domains".into(),
        input_schema: serde_json::json!({"type":"object"}),
        endpoint: "http://unused.local".into(),
        auth: ToolAuth::None,
        timeout_ms: Some(5_000),
        namespace: Some("aura_org_tools".into()),
        required_integration: None,
        runtime_execution: Some(InstalledToolRuntimeExecution::AppProvider(
            InstalledToolRuntimeProviderExecution {
                provider: "resend".into(),
                base_url: endpoint,
                static_headers: std::collections::HashMap::new(),
                integrations: vec![InstalledToolRuntimeIntegration {
                    integration_id: "resend-default".into(),
                    base_url: None,
                    auth: InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "resend-secret".into(),
                    },
                    provider_config: std::collections::HashMap::new(),
                }],
            },
        )),
        metadata: std::collections::HashMap::new(),
    }]);
    let (ctx, _dir) = test_context();
    let tc = ToolCall::new("resend_list_domains", serde_json::json!({}));
    let action = Action::delegate_tool(&tc).unwrap();
    let effect = resolver.execute(&ctx, &action).await.unwrap();
    assert_eq!(effect.status, EffectStatus::Committed);
    let result: ToolResult = serde_json::from_slice(&effect.payload).unwrap();
    let stdout = std::str::from_utf8(&result.stdout).unwrap();
    assert!(stdout.contains("\"example.com\""), "stdout was: {stdout}");
}

mod stub_domain {
    use async_trait::async_trait;
    use aura_tools_domain::*;

    pub struct StubDomainApi;

    #[async_trait]
    impl DomainApi for StubDomainApi {
        async fn list_specs(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<SpecDescriptor>> {
            Ok(vec![])
        }
        async fn get_spec(&self, _: &str, _: Option<&str>) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("stub")
        }
        async fn create_spec(
            &self,
            _: &str,
            title: &str,
            _: &str,
            _: u32,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            Ok(SpecDescriptor {
                id: "s1".into(),
                project_id: "p1".into(),
                title: title.into(),
                content: String::new(),
                order: 0,
                parent_id: None,
                content_hash: None,
            })
        }
        async fn update_spec(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("stub")
        }
        async fn delete_spec(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_tasks(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<TaskDescriptor>> {
            Ok(vec![])
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
            anyhow::bail!("stub")
        }
        async fn update_task(
            &self,
            _: &str,
            _: TaskUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("stub")
        }
        async fn delete_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn transition_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("stub")
        }
        async fn claim_next_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<Option<TaskDescriptor>> {
            Ok(None)
        }
        async fn get_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("stub")
        }
        async fn get_project(
            &self,
            project_id: &str,
            _: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            Ok(ProjectDescriptor {
                id: project_id.into(),
                name: "test".into(),
                path: String::new(),
                description: None,
                tech_stack: None,
                build_command: None,
                test_command: None,
            })
        }
        async fn update_project(
            &self,
            _: &str,
            _: ProjectUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            anyhow::bail!("stub")
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
            Ok(serde_json::json!({}))
        }
        async fn list_logs(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<u64>,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!([]))
        }
        async fn get_project_stats(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        async fn list_messages(&self, _: &str, _: &str) -> anyhow::Result<Vec<MessageDescriptor>> {
            Ok(vec![])
        }
        async fn save_message(&self, _: SaveMessageParams) -> anyhow::Result<()> {
            Ok(())
        }
        async fn create_session(
            &self,
            _: CreateSessionParams,
        ) -> anyhow::Result<SessionDescriptor> {
            anyhow::bail!("stub")
        }
        async fn get_active_session(&self, _: &str) -> anyhow::Result<Option<SessionDescriptor>> {
            Ok(None)
        }
        async fn orbit_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok("{}".into())
        }
        async fn network_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok("{}".into())
        }
    }

    use crate::domain_tools as aura_tools_domain;
}

#[tokio::test]
async fn domain_tool_succeeds_with_inaccessible_workspace() {
    use crate::domain_tools::DomainToolExecutor;

    let cat = Arc::new(ToolCatalog::new());
    let resolver = ToolResolver::new(cat, ToolConfig::default()).with_domain_executor(Arc::new(
        DomainToolExecutor::new(Arc::new(stub_domain::StubDomainApi)),
    ));

    let ctx = ExecuteContext::new(
        AgentId::generate(),
        ActionId::generate(),
        std::path::PathBuf::from("/nonexistent/impossible/workspace"),
    );

    let tc = ToolCall::new(
        "create_spec",
        serde_json::json!({
            "project_id": "p1",
            "title": "Hello World",
            "content": "# Hello"
        }),
    );
    let result = resolver.execute_tool(&ctx, &tc).await;
    assert!(
        result.is_ok(),
        "domain tool should succeed even with inaccessible workspace"
    );
    let tr = result.unwrap();
    let stdout = std::str::from_utf8(&tr.stdout).unwrap();
    assert!(
        stdout.contains("\"ok\":true"),
        "create_spec should return ok:true, got: {stdout}"
    );
}

#[tokio::test]
async fn get_project_succeeds_with_inaccessible_workspace() {
    use crate::domain_tools::DomainToolExecutor;

    let cat = Arc::new(ToolCatalog::new());
    let resolver = ToolResolver::new(cat, ToolConfig::default()).with_domain_executor(Arc::new(
        DomainToolExecutor::new(Arc::new(stub_domain::StubDomainApi)),
    ));

    let ctx = ExecuteContext::new(
        AgentId::generate(),
        ActionId::generate(),
        std::path::PathBuf::from("/nonexistent/impossible/workspace"),
    );

    let tc = ToolCall::new("get_project", serde_json::json!({"project_id": "p1"}));
    let result = resolver.execute_tool(&ctx, &tc).await;
    assert!(
        result.is_ok(),
        "get_project should succeed even with inaccessible workspace"
    );
    let tr = result.unwrap();
    let stdout = std::str::from_utf8(&tr.stdout).unwrap();
    assert!(
        stdout.contains("\"ok\":true"),
        "get_project should return ok:true, got: {stdout}"
    );
}

#[test]
fn every_exposed_core_tool_has_handler() {
    let (_cat, resolver) = make_catalog_and_resolver();
    let core = _cat.tools_for_profile(ToolProfile::Core);
    for t in &core {
        let has_handler = resolver.inner.has_tool(&t.name);
        assert!(
            has_handler,
            "core tool '{}' has no built-in handler",
            t.name,
        );
    }
}
