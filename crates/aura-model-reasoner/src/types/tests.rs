use super::*;

#[test]
fn test_message_user() {
    let msg = Message::user("Hello");
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.text_content(), "Hello");
}

#[test]
fn test_message_assistant() {
    let msg = Message::assistant("Hi there");
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.text_content(), "Hi there");
}

#[test]
fn test_message_tool_results() {
    let results = vec![
        ("id1".to_string(), ToolResultContent::text("result1"), false),
        ("id2".to_string(), ToolResultContent::text("error"), true),
    ];
    let msg = Message::tool_results(results);
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content.len(), 2);
}

#[test]
fn test_tool_definition() {
    let tool = ToolDefinition::new(
        "fs.read",
        "Read a file",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        }),
    );
    assert_eq!(tool.name, "fs.read");
}

#[test]
fn test_model_request_builder() {
    let request = ModelRequest::builder("claude-opus-4-6", "You are helpful")
        .message(Message::user("Hi"))
        .max_tokens(1000)
        .temperature(0.7)
        .request_kind(ModelRequestKind::Chat)
        .try_build()
        .unwrap();

    assert_eq!(request.model.as_str(), "claude-opus-4-6");
    assert_eq!(request.system, "You are helpful");
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.max_tokens.get(), 1000);
    assert_eq!(request.temperature.map(f32::from), Some(0.7));
    assert_eq!(request.metadata.kind, Some(ModelRequestKind::Chat));
}

#[test]
fn test_safe_auxiliary_request_contract_is_accepted() {
    let request = ModelRequest::builder("claude-opus-4-6", "You are helpful")
        .message(Message::user("Hi"))
        .try_build()
        .unwrap();

    let profile = ModelContentProfile::from_request(&request)
        .validate()
        .expect("safe untagged requests should remain accepted");

    assert_eq!(profile.kind, ModelRequestKind::Auxiliary);
    assert_eq!(profile.verdict, ModelContractVerdict::Accept);
}

#[test]
fn test_project_tool_task_extract_missing_stable_session_id_is_blocked() {
    let create_task = ToolDefinition::new(
        "create_task",
        "Create a task",
        serde_json::json!({"type": "object"}),
    );
    let request = ModelRequest::builder("claude-opus-4-6", "Extract project tasks")
        .message(Message::user("Turn this spec into tasks"))
        .tools(vec![create_task])
        .aura_project_id(Some("project-123".to_string()))
        .aura_agent_id(Some("agent-123".to_string()))
        .aura_org_id(Some("org-123".to_string()))
        .try_build()
        .unwrap();

    let violation = ModelContentProfile::from_request(&request)
        .validate()
        .expect_err("project task extraction requires a stable session id");

    assert_eq!(
        violation.reason,
        ModelContractViolationReason::MissingStableSessionId
    );
    assert_eq!(
        violation.profile.kind,
        ModelRequestKind::ProjectToolTaskExtract
    );
}

#[test]
fn test_project_tool_task_extract_accepts_benchmark_sized_requirements() {
    let create_task = ToolDefinition::new(
        "create_task",
        "Create a task",
        serde_json::json!({"type": "object"}),
    );
    let request = ModelRequest::builder("claude-opus-4-6", "Extract project tasks")
        .message(Message::user("x".repeat(24 * 1024)))
        .tools(vec![create_task])
        .aura_project_id(Some("project-123".to_string()))
        .aura_agent_id(Some("agent-123".to_string()))
        .aura_session_id(Some("session-123".to_string()))
        .aura_org_id(Some("org-123".to_string()))
        .try_build()
        .unwrap();

    let profile = ModelContentProfile::from_request(&request)
        .validate()
        .expect("SWE-bench requirements files around 20KiB should pass locally");

    assert_eq!(profile.kind, ModelRequestKind::ProjectToolTaskExtract);
    assert_eq!(profile.verdict, ModelContractVerdict::Accept);
}

#[test]
fn test_project_tool_task_extract_still_blocks_unbounded_context() {
    let create_task = ToolDefinition::new(
        "create_task",
        "Create a task",
        serde_json::json!({"type": "object"}),
    );
    let request = ModelRequest::builder("claude-opus-4-6", "Extract project tasks")
        .message(Message::user("x".repeat(49 * 1024)))
        .tools(vec![create_task])
        .aura_project_id(Some("project-123".to_string()))
        .aura_agent_id(Some("agent-123".to_string()))
        .aura_session_id(Some("session-123".to_string()))
        .aura_org_id(Some("org-123".to_string()))
        .try_build()
        .unwrap();

    let violation = ModelContentProfile::from_request(&request)
        .validate()
        .expect_err("truly oversized project-tool context should still be rejected locally");

    assert_eq!(
        violation.reason,
        ModelContractViolationReason::EmergencyCapRequired
    );
    assert_eq!(
        violation.profile.kind,
        ModelRequestKind::ProjectToolTaskExtract
    );
}

#[test]
fn test_oversized_dev_loop_bootstrap_is_blocked_before_emergency_cap() {
    let request = ModelRequest::builder("claude-opus-4-6", "You are an agent")
        .message(Message::user("x".repeat(20 * 1024)))
        .request_kind(ModelRequestKind::DevLoopBootstrap)
        .aura_project_id(Some("project-123".to_string()))
        .aura_agent_id(Some("agent-123".to_string()))
        .aura_session_id(Some("session-123".to_string()))
        .aura_org_id(Some("org-123".to_string()))
        .try_build()
        .unwrap();

    let violation = ModelContentProfile::from_request(&request)
        .validate()
        .expect_err("oversized bootstrap user content should be rejected locally");

    assert_eq!(
        violation.reason,
        ModelContractViolationReason::UnboundedBootstrapContext
    );
    assert_eq!(violation.profile.kind, ModelRequestKind::DevLoopBootstrap);
}

#[test]
fn test_content_block_serialization() {
    let text = ContentBlock::text("Hello");
    let json = serde_json::to_string(&text).unwrap();
    assert!(json.contains("\"type\":\"text\""));

    let tool_use = ContentBlock::tool_use("123", "fs.read", serde_json::json!({"path": "test"}));
    let json = serde_json::to_string(&tool_use).unwrap();
    assert!(json.contains("\"type\":\"tool_use\""));
}

#[test]
fn test_usage() {
    let usage = Usage::new(100, 50);
    assert_eq!(usage.total(), 150);
}

// ========================================================================
// Message Tests
// ========================================================================

#[test]
fn test_message_with_multiple_content_blocks() {
    let msg = Message::new(
        Role::Assistant,
        vec![
            ContentBlock::text("Let me help you."),
            ContentBlock::tool_use(
                "tool1",
                "read_file",
                serde_json::json!({"path": "test.txt"}),
            ),
        ],
    );

    assert!(msg.has_tool_use());
    assert_eq!(msg.tool_uses().len(), 1);
    assert_eq!(msg.text_content(), "Let me help you.");
}

#[test]
fn test_message_text_content_concatenation() {
    let msg = Message::new(
        Role::Assistant,
        vec![ContentBlock::text("Hello "), ContentBlock::text("world!")],
    );

    assert_eq!(msg.text_content(), "Hello world!");
}

#[test]
fn test_message_no_tool_use() {
    let msg = Message::assistant("Just text");
    assert!(!msg.has_tool_use());
    assert!(msg.tool_uses().is_empty());
}

// ========================================================================
// ContentBlock Tests
// ========================================================================

#[test]
fn test_content_block_as_text() {
    let text = ContentBlock::text("hello");
    assert_eq!(text.as_text(), Some("hello"));

    let tool_use = ContentBlock::tool_use("id", "name", serde_json::json!({}));
    assert_eq!(tool_use.as_text(), None);
}

#[test]
fn test_content_block_tool_result() {
    let result = ContentBlock::tool_result("tool123", ToolResultContent::text("success"), false);

    match result {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "tool123");
            assert!(!is_error);
            if let ToolResultContent::Text(t) = content {
                assert_eq!(t, "success");
            } else {
                panic!("Expected Text content");
            }
        }
        _ => panic!("Expected ToolResult"),
    }
}

#[test]
fn test_thinking_block() {
    let thinking = ContentBlock::Thinking {
        thinking: "Let me think about this...".to_string(),
        signature: Some("sig123".to_string()),
    };

    let json = serde_json::to_string(&thinking).unwrap();
    assert!(json.contains("\"type\":\"thinking\""));
    assert!(json.contains("sig123"));
}

// ========================================================================
// ToolResultContent Tests
// ========================================================================

#[test]
fn test_tool_result_content_text() {
    let content = ToolResultContent::text("result");
    if let ToolResultContent::Text(t) = content {
        assert_eq!(t, "result");
    } else {
        panic!("Expected Text");
    }
}

#[test]
fn test_tool_result_content_json() {
    let content = ToolResultContent::json(serde_json::json!({"key": "value"}));
    if let ToolResultContent::Json(v) = content {
        assert_eq!(v["key"], "value");
    } else {
        panic!("Expected Json");
    }
}

#[test]
fn test_tool_result_content_from_string() {
    let content: ToolResultContent = "hello".into();
    if let ToolResultContent::Text(t) = content {
        assert_eq!(t, "hello");
    } else {
        panic!("Expected Text");
    }
}

#[test]
fn test_tool_result_content_from_value() {
    let content: ToolResultContent = serde_json::json!({"a": 1}).into();
    if let ToolResultContent::Json(v) = content {
        assert_eq!(v["a"], 1);
    } else {
        panic!("Expected Json");
    }
}

// ========================================================================
// ToolChoice Tests
// ========================================================================

#[test]
fn test_tool_choice_variants() {
    let auto = ToolChoice::Auto;
    let none = ToolChoice::None;
    let required = ToolChoice::Required;
    let specific = ToolChoice::tool("read_file");

    assert!(matches!(auto, ToolChoice::Auto));
    assert!(matches!(none, ToolChoice::None));
    assert!(matches!(required, ToolChoice::Required));
    assert!(matches!(specific, ToolChoice::Tool { name } if name == "read_file"));
}

// ========================================================================
// ModelRequest Builder Tests
// ========================================================================

#[test]
fn test_model_request_builder_with_tools() {
    let tool = ToolDefinition::new(
        "test_tool",
        "A test tool",
        serde_json::json!({"type": "object"}),
    );

    let request = ModelRequest::builder("model", "system")
        .tools(vec![tool])
        .tool_choice(ToolChoice::Required)
        .try_build()
        .unwrap();

    assert_eq!(request.tools.len(), 1);
    assert!(matches!(request.tool_choice, ToolChoice::Required));
}

#[test]
fn test_model_request_builder_defaults() {
    let request = ModelRequest::builder("model", "system")
        .try_build()
        .unwrap();

    assert_eq!(request.max_tokens.get(), 4096);
    assert!(request.temperature.is_none());
    assert!(matches!(request.tool_choice, ToolChoice::Auto));
    assert!(request.messages.is_empty());
    assert!(request.tools.is_empty());
}

// ========================================================================
// ModelResponse Tests
// ========================================================================

#[test]
fn test_model_response_wants_tool_use() {
    let response = ModelResponse::new(
        StopReason::ToolUse,
        Message::assistant(""),
        Usage::new(100, 50),
        ProviderTrace::new("model", 100),
    );

    assert!(response.wants_tool_use());
    assert!(!response.is_end_turn());
}

#[test]
fn test_model_response_end_turn() {
    let response = ModelResponse::new(
        StopReason::EndTurn,
        Message::assistant("Done"),
        Usage::new(100, 50),
        ProviderTrace::new("model", 100),
    );

    assert!(!response.wants_tool_use());
    assert!(response.is_end_turn());
}
