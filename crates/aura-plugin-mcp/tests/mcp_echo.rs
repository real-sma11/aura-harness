//! Phase 4c stdio round-trip integration test.
//!
//! Spawns the in-crate `echo_mcp_server` binary, sends a JSON-RPC
//! request through the stdio transport, and asserts the response
//! shape. The same test runs on Unix and Windows because the echo
//! server is pure Rust (no per-OS shell script fixtures).

use std::collections::BTreeMap;

use aura_plugin_mcp::{McpClient, McpConnectionManager, McpError, ServerConfig};

fn echo_binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_echo_mcp_server")
}

#[test]
fn echo_server_request_roundtrip() {
    let mut client =
        McpClient::spawn(echo_binary_path(), &[], &BTreeMap::new()).expect("spawn echo server");
    let result = client
        .request("ping", &serde_json::json!({"hello": "world"}))
        .expect("request ok");
    assert_eq!(
        result.get("echoed").and_then(serde_json::Value::as_bool),
        Some(true),
        "echo server must set result.echoed = true; got {result:?}"
    );
    assert_eq!(
        result
            .get("params")
            .and_then(|p| p.get("hello"))
            .and_then(serde_json::Value::as_str),
        Some("world"),
        "echo server must round-trip params verbatim; got {result:?}"
    );
}

#[test]
fn echo_server_multiple_sequential_requests() {
    let mut client =
        McpClient::spawn(echo_binary_path(), &[], &BTreeMap::new()).expect("spawn echo server");
    for i in 0..3 {
        let result = client
            .request("ping", &serde_json::json!({"seq": i}))
            .expect("request ok");
        assert_eq!(
            result
                .get("params")
                .and_then(|p| p.get("seq"))
                .and_then(serde_json::Value::as_i64),
            Some(i),
        );
    }
}

#[test]
fn manager_registers_and_runs_request() {
    let mgr = McpConnectionManager::new();
    mgr.register(ServerConfig {
        server_id: "echo".into(),
        command: echo_binary_path().to_string(),
        args: vec![],
        env: BTreeMap::new(),
    })
    .expect("register echo server");
    assert!(mgr.contains("echo"));

    let result = mgr
        .with_client("echo", |client| {
            client.request("ping", &serde_json::json!({}))
        })
        .expect("with_client ok");
    assert_eq!(
        result.get("echoed").and_then(serde_json::Value::as_bool),
        Some(true),
    );
}

#[test]
fn manager_rejects_duplicate_server_id() {
    let mgr = McpConnectionManager::new();
    mgr.register(ServerConfig {
        server_id: "echo".into(),
        command: echo_binary_path().to_string(),
        args: vec![],
        env: BTreeMap::new(),
    })
    .expect("first register ok");

    // Second registration must surface DuplicateServer; the
    // first-active-wins merge means the original client keeps the
    // slot.
    let err = mgr
        .register(ServerConfig {
            server_id: "echo".into(),
            command: echo_binary_path().to_string(),
            args: vec![],
            env: BTreeMap::new(),
        })
        .expect_err("duplicate register must fail");
    assert!(matches!(err, McpError::DuplicateServer(id) if id == "echo"));
}
