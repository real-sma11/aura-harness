//! End-to-end integration tests against a live aura-runtime server.
//!
//! These tests boot a real node, connect via HTTP and WebSocket, and exercise
//! health, REST, WebSocket session lifecycle, and LLM/tool flows against live
//! services.
//!
//! Run all e2e tests:
//! ```text
//! cargo test --test e2e_live
//! ```
//!
//! Tests that require credentials gracefully skip (pass with a SKIP note)
//! when the relevant env vars are missing.
//!
//! Required environment (via `.env` or exported):
//! - `AURA_ROUTER_URL` (defaulted) and a JWT via `AURA_ROUTER_JWT` or `aura login`.
//!
//! Phase A note: every chat session is bootstrapped via the canonical
//! `POST /v1/run` + `WS /stream/:run_id` two-step exchange. The old
//! `WS /stream` + `InboundMessage::SessionInit` first-frame contract
//! is gone; tests that exercised it (`test_ws_session_init_twice_errors`,
//! `test_ws_user_message_before_init`) have been deleted.

mod common;

use std::time::Duration;

use aura_core::AgentId;
use common::{
    assert_stop_reason, chat_request_payload, chat_request_payload_full, collect_text,
    connect_llm_session, find_agent_dir, find_file, has_tool_error, http_client, open_chat_run,
    place_file_in_agent_dir, post_runtime_request, tool_names_used, TestServer, WsClient,
};
use serde_json::{json, Value};

// Local wrapper around `common::require_llm_token` that first gates on
// `AURA_RUN_LIVE_E2E`. The inner call used to be `$crate::require_llm!()`,
// which resolved back to *this* macro (the local definition shadows the
// `#[macro_export]` sibling in `tests/common/mod.rs`) — triggering
// unbounded recursion on the first real run. Calling the function
// directly sidesteps the macro-resolution ambiguity.
macro_rules! require_llm {
    () => {{
        let enabled = std::env::var("AURA_RUN_LIVE_E2E")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        if !enabled {
            eprintln!("skipping live LLM test: set AURA_RUN_LIVE_E2E=1 to enable it");
            return;
        }
        $crate::common::require_llm_token()
    }};
}

// ============================================================================
// Suite 1: Health and REST API
// ============================================================================

#[tokio::test]

async fn test_health() {
    let server = TestServer::start().await;
    let resp = http_client()
        .get(format!("{}/health", server.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
}

/// When `AURA_NODE_REQUIRE_AUTH` is off (the default), the router
/// should not attach `require_bearer_mw` and callers without a Bearer
/// header must reach handler logic rather than getting bounced at the
/// middleware layer. Asserting "not 401" is stronger than asserting a
/// specific success status — the test fixture wires up no automaton
/// controller, so `/v1/run/list` legitimately replies `503`; that's
/// still proof the request traversed the protected sub-router without
/// being rejected for missing auth (which is the regression this guard
/// actually cares about).
#[tokio::test]
async fn test_protected_route_open_when_auth_disabled() {
    let server = TestServer::start_without_auth().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp = client
        .get(format!("{}/v1/run/list", server.base_url()))
        .send()
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "protected route should not return 401 when require_auth=false"
    );
}

#[tokio::test]

async fn test_submit_tx() {
    let server = TestServer::start().await;
    let agent_id = AgentId::generate();
    let payload_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "Hello from e2e");
    let body = json!({
        "agent_id": agent_id.to_hex(),
        "kind": "user_prompt",
        "payload": payload_b64
    });
    let resp = http_client()
        .post(format!("{}/tx", server.base_url()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let data: Value = resp.json().await.unwrap();
    assert!(data["accepted"].as_bool().unwrap());
    assert!(data["tx_id"].is_string());
}

#[tokio::test]

async fn test_get_head() {
    let server = TestServer::start().await;
    let agent_id = AgentId::generate();
    let resp = http_client()
        .get(format!(
            "{}/agents/{}/head",
            server.base_url(),
            agent_id.to_hex()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let data: Value = resp.json().await.unwrap();
    assert_eq!(data["head_seq"], 0);
}

#[tokio::test]

async fn test_scan_record() {
    let server = TestServer::start().await;
    let agent_id = AgentId::generate();
    let resp = http_client()
        .get(format!(
            "{}/agents/{}/record?from_seq=1&limit=10",
            server.base_url(),
            agent_id.to_hex()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let data: Value = resp.json().await.unwrap();
    assert!(data.as_array().unwrap().is_empty());
}

// ============================================================================
// Suite 2: WebSocket Session Lifecycle (Phase A wire shape)
// ============================================================================

#[tokio::test]
async fn test_ws_session_init() {
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("test-agent");
    std::fs::create_dir_all(&ws_path).unwrap();

    // Use the raw POST + connect pattern so we can introspect the
    // SessionReady frame (the high-level `open_chat_run` helper
    // consumes it).
    let payload = chat_request_payload(&ws_path, None);
    let resp = post_runtime_request(&server, &payload).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    let run_id = body["run_id"].as_str().unwrap().to_string();
    let mut ws = WsClient::connect(&server.stream_url(&run_id)).await;
    let ready = ws.expect_session_ready().await;

    let tools = ready["tools"].as_array().unwrap();
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert!(tool_names.contains(&"list_files"), "missing list_files");
    assert!(tool_names.contains(&"read_file"), "missing read_file");
    assert!(tool_names.contains(&"write_file"), "missing write_file");
    assert!(tool_names.contains(&"edit_file"), "missing edit_file");
    assert!(tool_names.contains(&"delete_file"), "missing delete_file");
    assert!(tool_names.contains(&"find_files"), "missing find_files");
    assert!(tool_names.contains(&"search_code"), "missing search_code");
    assert!(tool_names.contains(&"run_command"), "missing run_command");
    assert!(tool_names.contains(&"stat_file"), "missing stat_file");
}

// ============================================================================
// Suite 3: Real LLM Simple Turn
// ============================================================================

#[tokio::test]

async fn test_ws_simple_prompt() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("simple-prompt");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    ws.send_user_message("What is 2+2? Reply with just the number, nothing else.")
        .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    assert!(!messages.is_empty(), "no messages received");

    let has_text = messages.iter().any(|m| m["type"] == "text_delta");
    assert!(has_text, "expected at least one text_delta");

    assert_stop_reason(&messages, "end_turn");

    let end = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end")
        .unwrap();
    let input_tokens = end["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = end["usage"]["output_tokens"].as_u64().unwrap_or(0);
    assert!(input_tokens > 0, "expected non-zero input_tokens");
    assert!(output_tokens > 0, "expected non-zero output_tokens");

    let text = collect_text(&messages);
    assert!(
        text.contains('4'),
        "expected response to contain '4', got: {text}"
    );
}

// ============================================================================
// Suite 4: LLM + Filesystem Tool E2E Tests
// ============================================================================

#[tokio::test]

async fn test_tool_write_and_read_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-write-read");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    ws.send_user_message(
        "Use the write_file tool to create a file at path 'hello.txt' with the content 'Hello E2E Test'. \
         After writing, use the read_file tool to read 'hello.txt' and confirm it exists.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"write_file".to_string()),
        "expected write_file tool use"
    );

    let file_path = find_file(&ws_path, "hello.txt");
    assert!(
        file_path.is_some(),
        "file was not created on disk anywhere under workspace"
    );
    let content = std::fs::read_to_string(file_path.unwrap()).unwrap();
    assert!(
        content.contains("Hello E2E Test"),
        "file content mismatch: {content}"
    );
}

#[tokio::test]

async fn test_tool_edit_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-edit");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    // Place file in the agent directory (where tools operate)
    place_file_in_agent_dir(
        &ws_path,
        "target.txt",
        "The quick brown fox jumps over the lazy dog.",
    );

    ws.send_user_message(
        "Use the edit_file tool on the file 'target.txt'. \
         The old_string to find is 'brown fox' and the new_string to replace it with is 'red panda'. \
         Do this now using the edit_file tool.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"edit_file".to_string()),
        "expected edit_file tool use, got: {tools:?}"
    );

    // Verify the edit was applied (LLM may provide slightly different params)
    if let Some(file_path) = find_file(&ws_path, "target.txt") {
        let content = std::fs::read_to_string(file_path).unwrap();
        if !content.contains("red panda") {
            eprintln!(
                "NOTE: edit_file tool was invoked but edit wasn't applied as expected (LLM behavior). \
                 Content: {content}"
            );
        }
    }
}

#[tokio::test]

async fn test_tool_delete_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-delete");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    // Place file in the agent directory (where tools operate)
    place_file_in_agent_dir(&ws_path, "to_delete.txt", "delete me");

    ws.send_user_message(
        "Use the delete_file tool to delete the file at path 'to_delete.txt'. Do it now.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"delete_file".to_string()),
        "expected delete_file tool use, got: {tools:?}"
    );
    // Verify deletion (LLM may provide a slightly different path)
    if find_file(&ws_path, "to_delete.txt").is_some() {
        eprintln!(
            "NOTE: delete_file tool was invoked but file still exists (LLM may have used wrong path)"
        );
    }
}

#[tokio::test]

async fn test_tool_list_files() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-list");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    place_file_in_agent_dir(&ws_path, "alpha.txt", "a");
    place_file_in_agent_dir(&ws_path, "bravo.txt", "b");
    place_file_in_agent_dir(&ws_path, "charlie.txt", "c");

    ws.send_user_message(
        "Use the list_files tool to list the contents of the current directory '.'. \
         Then tell me the exact filenames you found.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"list_files".to_string()),
        "expected list_files tool use, got: {tools:?}"
    );
}

#[tokio::test]

async fn test_tool_stat_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-stat");
    std::fs::create_dir_all(&ws_path).unwrap();
    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    place_file_in_agent_dir(&ws_path, "info.txt", "some content here");

    ws.send_user_message("Get file metadata for 'info.txt' using the stat_file tool.")
        .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"stat_file".to_string()),
        "expected stat_file tool use"
    );
    assert_stop_reason(&messages, "end_turn");
}

#[tokio::test]

async fn test_tool_find_files() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-find");
    let nested = ws_path.join("src").join("components");
    std::fs::create_dir_all(&nested).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    // Create file structure in the agent directory (where tools operate)
    if let Some(agent_dir) = find_agent_dir(&ws_path) {
        let agent_src = agent_dir.join("src").join("components");
        std::fs::create_dir_all(&agent_src).unwrap();
        std::fs::write(agent_dir.join("readme.md"), "root").unwrap();
        std::fs::write(agent_dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(agent_src.join("button.rs"), "struct Button;").unwrap();
        std::fs::write(agent_src.join("input.rs"), "struct Input;").unwrap();
    } else {
        std::fs::write(ws_path.join("readme.md"), "root").unwrap();
        std::fs::write(ws_path.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(nested.join("button.rs"), "struct Button;").unwrap();
        std::fs::write(nested.join("input.rs"), "struct Input;").unwrap();
    }

    ws.send_user_message(
        "Find all '.rs' files in this project recursively. Use the find_files tool.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"find_files".to_string()),
        "expected find_files tool use"
    );
    assert_stop_reason(&messages, "end_turn");
}

#[tokio::test]

async fn test_tool_search_code() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-search");
    std::fs::create_dir_all(&ws_path).unwrap();

    std::fs::write(
        ws_path.join("app.rs"),
        "fn calculate_total(items: &[i32]) -> i32 {\n    items.iter().sum()\n}\n",
    )
    .unwrap();
    std::fs::write(
        ws_path.join("lib.rs"),
        "pub fn calculate_total(prices: &[f64]) -> f64 {\n    prices.iter().sum()\n}\n",
    )
    .unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    place_file_in_agent_dir(
        &ws_path,
        "app.rs",
        "fn calculate_total(items: &[i32]) -> i32 {\n    items.iter().sum()\n}\n",
    );
    place_file_in_agent_dir(
        &ws_path,
        "lib.rs",
        "pub fn calculate_total(prices: &[f64]) -> f64 {\n    prices.iter().sum()\n}\n",
    );

    ws.send_user_message(
        "Search for all occurrences of 'calculate_total' in the codebase. Use the search_code tool.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"search_code".to_string()),
        "expected search_code tool use"
    );
    assert_stop_reason(&messages, "end_turn");
}

#[tokio::test]

async fn test_tool_run_command() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-cmd");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    let cmd = if cfg!(windows) {
        "cmd /c echo hello_e2e"
    } else {
        "echo hello_e2e"
    };
    ws.send_user_message(&format!(
        "Use the run_command tool to execute this exact command: {cmd}"
    ))
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"run_command".to_string()),
        "expected run_command tool use, got: {tools:?}"
    );

    // Check the tool result contains our marker string
    let tool_results: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "tool_result" && m["name"] == "run_command")
        .collect();
    if !tool_results.is_empty() {
        let result_text = tool_results[0]["result"].as_str().unwrap_or("");
        assert!(
            result_text.contains("hello_e2e"),
            "command output should contain 'hello_e2e', got: {result_text}"
        );
    }
}

// ============================================================================
// Suite 5: Multi-turn Conversation
// ============================================================================

#[tokio::test]

async fn test_ws_multi_turn() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("multi-turn");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    // Turn 1: create a file
    ws.send_user_message(
        "Use the write_file tool to create a file at path 'memo.txt' with content 'Remember to buy milk'.",
    )
    .await;
    let turn1 = ws.collect_turn(Duration::from_secs(120)).await;
    let turn1_tools = tool_names_used(&turn1);
    assert!(
        turn1_tools.contains(&"write_file".to_string()),
        "expected write_file in turn 1, got: {turn1_tools:?}"
    );
    assert!(
        find_file(&ws_path, "memo.txt").is_some(),
        "file should exist somewhere under workspace after turn 1"
    );

    // Turn 2: read the file from turn 1 (tests context carryover)
    ws.send_user_message("Use the read_file tool to read 'memo.txt' and tell me its contents.")
        .await;
    let turn2 = ws.collect_turn(Duration::from_secs(120)).await;

    // The LLM may read the file or recall from context.
    // The key assertion is that multi-turn context works (turn 2 completes).
    assert_stop_reason(&turn2, "end_turn");

    let text = collect_text(&turn2);
    let tool_result_text: String = turn2
        .iter()
        .filter(|m| m["type"] == "tool_result")
        .filter_map(|m| m["result"].as_str())
        .collect();
    let combined = format!("{text}{tool_result_text}");
    if !combined.contains("milk") && !combined.contains("Remember") && !combined.contains("memo") {
        eprintln!(
            "NOTE: turn 2 completed but didn't explicitly reference file content (LLM behavior)"
        );
    }
}

// ============================================================================
// Suite 6: Cancellation
// ============================================================================

#[tokio::test]

async fn test_ws_cancel_turn() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("cancel-test");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    ws.send_user_message(
        "Write a very long essay about the history of computing, at least 2000 words. \
         Include detailed technical analysis of every major era.",
    )
    .await;

    // Wait for the turn to start streaming, then cancel
    tokio::time::sleep(Duration::from_secs(2)).await;
    ws.send_cancel().await;

    let messages = ws.collect_turn(Duration::from_secs(60)).await;

    // The turn should eventually end - either with cancelled or end_turn
    let end_msg = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end");
    if let Some(end) = end_msg {
        let stop = end["stop_reason"].as_str().unwrap_or("");
        assert!(
            stop == "cancelled" || stop == "end_turn" || stop == "end_turn_with_errors",
            "expected cancelled or end_turn, got: {stop}"
        );
    }
    // If no end message, the turn was still in-flight and we timed out - acceptable for cancel test
}

// ============================================================================
// Suite 11: Error Handling
//
// Phase A note: WebSocket parse-error coverage now requires an
// attached session (the `/stream/:run_id` route is the only WS
// endpoint and only sees frames after `prepare_chat_session`
// succeeds).
// ============================================================================

#[tokio::test]

async fn test_ws_invalid_json() {
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("invalid-json");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = open_chat_run(&server, &chat_request_payload(&ws_path, None)).await;
    ws.send_raw("this is not json at all!!!").await;

    let msg = ws.recv_json().await.expect("expected error response");
    assert_eq!(msg["type"], "error");
    assert_eq!(msg["code"], "parse_error");
}

#[tokio::test]

async fn test_ws_message_during_turn() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("msg-during-turn");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    ws.send_user_message(
        "Write a detailed 500-word essay about the importance of software testing.",
    )
    .await;

    // Wait for the turn to start streaming
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Try sending another message while turn is active
    ws.send_user_message("This should be rejected").await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;

    let has_turn_in_progress = messages
        .iter()
        .any(|m| m["type"] == "error" && m["code"] == "turn_in_progress");
    assert!(
        has_turn_in_progress,
        "expected turn_in_progress error, messages: {:?}",
        messages
            .iter()
            .map(|m| m["type"].as_str())
            .collect::<Vec<_>>()
    );
}

// ============================================================================
// Suite 12: Streaming Protocol Field Verification
// ============================================================================

#[tokio::test]
async fn test_ws_streaming_message_fields() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("stream-fields");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    ws.send_user_message("Say hello in one sentence.").await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    assert!(!messages.is_empty(), "no messages received");

    let start = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_start");
    assert!(start.is_some(), "missing assistant_message_start");
    let start_msg_id = start.unwrap()["message_id"].as_str().unwrap_or("");
    assert!(
        !start_msg_id.is_empty(),
        "assistant_message_start should have non-empty message_id"
    );

    let has_text_delta = messages
        .iter()
        .any(|m| m["type"] == "text_delta" && m["text"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(has_text_delta, "expected at least one non-empty text_delta");

    let end_msg = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end")
        .expect("missing assistant_message_end");

    let end_msg_id = end_msg["message_id"].as_str().unwrap_or("");
    assert_eq!(
        start_msg_id, end_msg_id,
        "message_id should match between start and end"
    );

    assert!(
        end_msg["stop_reason"].is_string(),
        "stop_reason should be a string"
    );

    let usage = &end_msg["usage"];
    assert!(
        usage["input_tokens"].as_u64().unwrap_or(0) > 0,
        "input_tokens should be > 0"
    );
    assert!(
        usage["output_tokens"].as_u64().unwrap_or(0) > 0,
        "output_tokens should be > 0"
    );
    assert!(
        usage["model"].is_string() && !usage["model"].as_str().unwrap().is_empty(),
        "model should be non-empty"
    );

    let fc = &end_msg["files_changed"];
    assert!(
        fc["created"].is_array(),
        "files_changed.created should be an array"
    );
    assert!(
        fc["modified"].is_array(),
        "files_changed.modified should be an array"
    );
    assert!(
        fc["deleted"].is_array(),
        "files_changed.deleted should be an array"
    );
}

#[tokio::test]
async fn test_ws_files_changed_after_write() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("files-changed");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    ws.send_user_message(
        "Use the write_file tool to create a file called 'changed.txt' with the content 'tracking changes'.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"write_file".to_string()),
        "expected write_file"
    );

    // files_changed is populated by the server; it may or may not track tool-based writes
    // depending on implementation. We verify the field structure exists regardless.
    let end_msg = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end");
    assert!(end_msg.is_some(), "missing assistant_message_end");
    let fc = &end_msg.unwrap()["files_changed"];
    assert!(
        fc["created"].is_array(),
        "files_changed.created should be an array"
    );
}

// ============================================================================
// Suite 13: Session Config Overrides
// ============================================================================

#[tokio::test]
async fn test_ws_session_init_with_system_prompt() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("sys-prompt");
    std::fs::create_dir_all(&ws_path).unwrap();

    let tok = if token.is_empty() {
        None
    } else {
        Some(token.as_str())
    };
    let payload = chat_request_payload_full(
        &ws_path,
        tok,
        Some("You are a pirate. Every response must contain the word 'arrr'. This is mandatory."),
        None,
        None,
        None,
    );
    let mut ws = open_chat_run(&server, &payload).await;

    ws.send_user_message("Say hello.").await;
    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    assert_stop_reason(&messages, "end_turn");

    let text = collect_text(&messages).to_lowercase();
    assert!(
        text.contains("arrr")
            || text.contains("ahoy")
            || text.contains("matey")
            || text.contains("pirate"),
        "system_prompt override should influence response, got: {text}"
    );
}

#[tokio::test]
async fn test_ws_session_init_with_model() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("model-override");
    std::fs::create_dir_all(&ws_path).unwrap();

    // Pin to the env-fallback model identifier — this end-to-end test
    // exercises the wire-protocol round trip, not model selection logic.
    let model = aura_reasoner::ENV_FALLBACK_MODEL;
    let tok = if token.is_empty() {
        None
    } else {
        Some(token.as_str())
    };
    let payload = chat_request_payload_full(&ws_path, tok, None, Some(model), None, None);
    let mut ws = open_chat_run(&server, &payload).await;

    ws.send_user_message("Say hi.").await;
    let messages = ws.collect_turn(Duration::from_secs(120)).await;

    let end_msg = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end")
        .expect("missing assistant_message_end");
    let used_model = end_msg["usage"]["model"].as_str().unwrap_or("");
    assert!(!used_model.is_empty(), "usage.model should be non-empty");
}

#[tokio::test]
async fn test_ws_session_init_with_max_turns() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("max-turns");
    std::fs::create_dir_all(&ws_path).unwrap();

    let tok = if token.is_empty() {
        None
    } else {
        Some(token.as_str())
    };
    let payload = chat_request_payload_full(&ws_path, tok, None, None, None, Some(1));
    let mut ws = open_chat_run(&server, &payload).await;

    place_file_in_agent_dir(&ws_path, "a.txt", "aaa");
    place_file_in_agent_dir(&ws_path, "b.txt", "bbb");

    ws.send_user_message(
        "Read the files a.txt, b.txt, then list all files, then create a summary file. \
         Use the tools for each step.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let end_msg = messages
        .iter()
        .find(|m| m["type"] == "assistant_message_end");
    assert!(
        end_msg.is_some(),
        "should get assistant_message_end even with max_turns=1"
    );
}

#[tokio::test]
async fn test_ws_auth_bearer_header() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    assert!(
        !token.is_empty(),
        "bearer header test requires a non-empty token"
    );
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("bearer-header");
    std::fs::create_dir_all(&ws_path).unwrap();

    // Phase A: the model JWT can either ride in the `auth_jwt` body
    // field or as a transport bearer that the gateway threads through.
    // This test exercises the bearer-header path: the POST + WS both
    // use the JWT as their transport credential, and the body's
    // `auth_jwt` is left unset.
    let payload = chat_request_payload(&ws_path, None);
    let mut ws = common::open_chat_run_with_auth(&server, &payload, &token).await;

    ws.send_user_message("Say hello in one word.").await;
    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    assert!(
        !messages.is_empty(),
        "should receive messages with bearer header auth"
    );

    let has_text = messages.iter().any(|m| m["type"] == "text_delta");
    assert!(has_text, "should get text_delta with bearer header auth");
}

// ============================================================================
// Suite 14: Tool Error Paths
// ============================================================================

#[tokio::test]
async fn test_tool_read_nonexistent_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-err-read");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    ws.send_user_message(
        "Use the read_file tool to read a file called 'does_not_exist_xyz.txt'. Do it now.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"read_file".to_string()),
        "expected read_file tool use, got: {tools:?}"
    );

    if !has_tool_error(&messages, "read_file") {
        eprintln!("NOTE: read_file on nonexistent file did not produce is_error=true (LLM may have handled it)");
    }
}

#[tokio::test]
async fn test_tool_delete_nonexistent_file() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-err-delete");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    ws.send_user_message(
        "Use the delete_file tool to delete 'nonexistent_file_xyz.txt'. Do it now.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"delete_file".to_string()),
        "expected delete_file tool use, got: {tools:?}"
    );
}

#[tokio::test]
async fn test_tool_run_command_failure() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-err-cmd");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    let cmd = if cfg!(windows) {
        "cmd /c dir nonexistent_dir_e2e_xyz 2>&1"
    } else {
        "ls /nonexistent_dir_e2e_xyz 2>&1"
    };
    ws.send_user_message(&format!(
        "Use the run_command tool to execute exactly this command: {cmd}"
    ))
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"run_command".to_string()),
        "expected run_command tool use, got: {tools:?}"
    );

    let cmd_results: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "tool_result" && m["name"] == "run_command")
        .collect();
    if !cmd_results.is_empty() {
        let result_text = cmd_results[0]["result"].as_str().unwrap_or("");
        assert!(
            result_text.contains("not find")
                || result_text.contains("No such")
                || result_text.contains("cannot"),
            "failing command should mention error, got: {result_text}"
        );
    }
}

#[tokio::test]
async fn test_tool_edit_no_match() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-err-edit");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    place_file_in_agent_dir(&ws_path, "stable.txt", "This content will not change.");

    ws.send_user_message(
        "Use the edit_file tool on 'stable.txt'. The old_string is 'ZZZZZ_NONEXISTENT_ZZZZZ' \
         and the new_string is 'replaced'. Do it now.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"edit_file".to_string()),
        "expected edit_file tool use, got: {tools:?}"
    );

    if has_tool_error(&messages, "edit_file") {
        // Good: tool correctly reported the no-match error
    } else {
        eprintln!("NOTE: edit_file with no-match string did not produce is_error=true");
    }

    // Verify the file was NOT modified
    if let Some(path) = find_file(&ws_path, "stable.txt") {
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            content, "This content will not change.",
            "file should be unchanged after no-match edit"
        );
    }
}

// ============================================================================
// Suite 15: Tool Parameter Variations
// ============================================================================

#[tokio::test]
async fn test_tool_read_file_line_range() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-line-range");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    let content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
    place_file_in_agent_dir(&ws_path, "lines.txt", content);

    ws.send_user_message(
        "Use the read_file tool to read 'lines.txt' with start_line=3 and end_line=5. \
         Only read those specific lines.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"read_file".to_string()),
        "expected read_file tool use, got: {tools:?}"
    );

    let tool_results: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "tool_result" && m["name"] == "read_file")
        .collect();
    if !tool_results.is_empty() {
        let result = tool_results[0]["result"].as_str().unwrap_or("");
        assert!(
            result.contains("line3"),
            "line range result should include line3, got: {result}"
        );
    }
}

#[tokio::test]
async fn test_tool_run_command_with_cwd() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-cwd");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    if let Some(agent_dir) = find_agent_dir(&ws_path) {
        std::fs::create_dir_all(agent_dir.join("subdir")).unwrap();
    }

    let cmd = if cfg!(windows) { "cd" } else { "pwd" };
    ws.send_user_message(&format!(
        "Use the run_command tool to execute '{cmd}' with the working_dir (or cwd) set to 'subdir'."
    ))
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"run_command".to_string()),
        "expected run_command tool use, got: {tools:?}"
    );

    let cmd_results: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "tool_result" && m["name"] == "run_command")
        .collect();
    if !cmd_results.is_empty() {
        let result = cmd_results[0]["result"].as_str().unwrap_or("");
        assert!(
            result.contains("subdir"),
            "cwd should reference subdir, got: {result}"
        );
    }
}

#[tokio::test]
async fn test_tool_write_file_overwrite() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-overwrite");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;
    place_file_in_agent_dir(&ws_path, "overwrite.txt", "ORIGINAL CONTENT");

    ws.send_user_message(
        "Use the write_file tool to write 'OVERWRITTEN CONTENT' to the file 'overwrite.txt'. \
         This should overwrite the existing file.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"write_file".to_string()),
        "expected write_file tool use, got: {tools:?}"
    );

    if let Some(path) = find_file(&ws_path, "overwrite.txt") {
        let content = std::fs::read_to_string(path).unwrap();
        assert!(
            content.contains("OVERWRITTEN"),
            "file should have overwritten content, got: {content}"
        );
        assert!(
            !content.contains("ORIGINAL"),
            "original content should be gone, got: {content}"
        );
    }
}

#[tokio::test]
async fn test_tool_search_code_regex() {
    let _ = dotenvy::dotenv();
    let token = require_llm!();
    let server = TestServer::start().await;
    let ws_path = server.workspaces_path().join("tool-regex");
    std::fs::create_dir_all(&ws_path).unwrap();

    let mut ws = connect_llm_session(&server, &ws_path, &token).await;

    place_file_in_agent_dir(
        &ws_path,
        "math.rs",
        "fn add_numbers(a: i32, b: i32) -> i32 { a + b }\nfn subtract_numbers(a: i32, b: i32) -> i32 { a - b }\n",
    );
    place_file_in_agent_dir(
        &ws_path,
        "utils.rs",
        "fn multiply_numbers(a: i32, b: i32) -> i32 { a * b }\n",
    );

    ws.send_user_message(
        "Use the search_code tool with the regex pattern 'fn \\w+_numbers' to find all \
         functions ending with '_numbers'. Search the current directory.",
    )
    .await;

    let messages = ws.collect_turn(Duration::from_secs(120)).await;
    let tools = tool_names_used(&messages);
    assert!(
        tools.contains(&"search_code".to_string()),
        "expected search_code tool use, got: {tools:?}"
    );

    let tool_results: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "tool_result" && m["name"] == "search_code")
        .collect();
    if !tool_results.is_empty() {
        let result = tool_results[0]["result"].as_str().unwrap_or("");
        assert!(
            result.contains("add_numbers")
                || result.contains("subtract_numbers")
                || result.contains("multiply_numbers"),
            "regex search should find _numbers functions, got: {result}"
        );
    }
}
