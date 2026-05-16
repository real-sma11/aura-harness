use super::*;

// ========================================================================
// StreamAccumulator Tests
// ========================================================================

#[test]
fn test_stream_accumulator_text_only() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::MessageStart {
        message_id: "msg1".to_string(),
        model: "claude".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Hello ".to_string(),
    });
    acc.process(&StreamEvent::TextDelta {
        text: "world!".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });
    acc.process(&StreamEvent::MessageDelta {
        stop_reason: Some(StopReason::EndTurn),
        output_tokens: 10,
    });
    acc.process(&StreamEvent::MessageStop);

    assert_eq!(acc.message_id, "msg1");
    assert_eq!(acc.model, "claude");
    assert_eq!(acc.text_content, "Hello world!");
    assert_eq!(acc.output_tokens, 10);
    assert_eq!(acc.stop_reason, Some(StopReason::EndTurn));
}

#[test]
fn test_stream_accumulator_tool_use() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::MessageStart {
        message_id: "msg1".to_string(),
        model: "claude".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::ToolUse {
            id: "tool1".to_string(),
            name: "read_file".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"#.to_string(),
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#""test.txt"}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });
    acc.process(&StreamEvent::MessageDelta {
        stop_reason: Some(StopReason::ToolUse),
        output_tokens: 20,
    });

    assert_eq!(acc.tool_uses.len(), 1);
    assert_eq!(acc.tool_uses[0].id, "tool1");
    assert_eq!(acc.tool_uses[0].name, "read_file");
    assert_eq!(acc.tool_uses[0].input_json, r#"{"path":"test.txt"}"#);
}

#[test]
fn test_stream_accumulator_thinking() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Thinking,
    });
    acc.process(&StreamEvent::ThinkingDelta {
        thinking: "Let me ".to_string(),
    });
    acc.process(&StreamEvent::ThinkingDelta {
        thinking: "think...".to_string(),
    });
    acc.process(&StreamEvent::SignatureDelta {
        signature: "sig_abc".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });

    assert_eq!(acc.thinking_content, "Let me think...");
    assert_eq!(acc.thinking_signature, Some("sig_abc".to_string()));
}

#[test]
fn test_stream_accumulator_mixed_content() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Thinking,
    });
    acc.process(&StreamEvent::ThinkingDelta {
        thinking: "Thinking...".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });

    acc.process(&StreamEvent::ContentBlockStart {
        index: 1,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Response text".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 1 });

    acc.process(&StreamEvent::ContentBlockStart {
        index: 2,
        content_type: StreamContentType::ToolUse {
            id: "tool1".to_string(),
            name: "list_files".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"."}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 2 });

    assert_eq!(acc.thinking_content, "Thinking...");
    assert_eq!(acc.text_content, "Response text");
    assert_eq!(acc.tool_uses.len(), 1);
}

#[test]
fn test_stream_accumulator_into_response() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::MessageStart {
        message_id: "msg123".to_string(),
        model: "claude-opus-4-6".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Hello!".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });
    acc.process(&StreamEvent::MessageDelta {
        stop_reason: Some(StopReason::EndTurn),
        output_tokens: 5,
    });

    let response = acc.into_response(100, 500).unwrap();

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.message.text_content(), "Hello!");
    assert_eq!(response.usage.input_tokens, 100);
    assert_eq!(response.usage.output_tokens, 5);
    assert_eq!(response.trace.model, "claude-opus-4-6");
    assert_eq!(response.trace.latency_ms, 500);
}

#[test]
fn test_stream_accumulator_into_response_with_thinking() {
    let mut acc = StreamAccumulator::new();

    acc.thinking_content = "Deep thoughts...".to_string();
    acc.thinking_signature = Some("sig123".to_string());
    acc.text_content = "Here's my answer".to_string();
    acc.stop_reason = Some(StopReason::EndTurn);
    acc.model = "claude".to_string();

    let response = acc.into_response(50, 200).unwrap();

    assert_eq!(response.message.content.len(), 2);
    assert!(matches!(
        &response.message.content[0],
        ContentBlock::Thinking { .. }
    ));
    assert!(matches!(
        &response.message.content[1],
        ContentBlock::Text { .. }
    ));
}

#[test]
fn test_stream_accumulator_into_response_with_tool() {
    let mut acc = StreamAccumulator::new();

    acc.tool_uses.push(AccumulatedToolUse {
        id: "tool1".to_string(),
        name: "read_file".to_string(),
        input_json: r#"{"path":"test.txt"}"#.to_string(),
    });
    acc.stop_reason = Some(StopReason::ToolUse);

    let response = acc.into_response(50, 100).unwrap();

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert!(response.message.has_tool_use());

    if let ContentBlock::ToolUse { id, name, input } = &response.message.content[0] {
        assert_eq!(id, "tool1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "test.txt");
    } else {
        panic!("Expected ToolUse block");
    }
}

#[test]
fn test_stream_accumulator_invalid_json_handling() {
    let mut acc = StreamAccumulator::new();

    acc.tool_uses.push(AccumulatedToolUse {
        id: "tool1".to_string(),
        name: "test".to_string(),
        input_json: "invalid json {{{".to_string(),
    });

    let response = acc.into_response(0, 0).unwrap();

    if let ContentBlock::ToolUse { input, .. } = &response.message.content[0] {
        assert!(input.get("raw").is_some());
    } else {
        panic!("Expected ToolUse block");
    }
}

#[test]
fn test_stream_accumulator_empty_tool_json() {
    let mut acc = StreamAccumulator::new();

    acc.tool_uses.push(AccumulatedToolUse {
        id: "tool1".to_string(),
        name: "test".to_string(),
        input_json: String::new(),
    });

    let response = acc.into_response(0, 0).unwrap();

    if let ContentBlock::ToolUse { input, .. } = &response.message.content[0] {
        assert!(input.is_object());
        assert!(input.as_object().unwrap().is_empty());
    } else {
        panic!("Expected ToolUse block");
    }
}

#[test]
fn test_stream_accumulator_multiple_tools() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::ToolUse {
            id: "tool1".to_string(),
            name: "list_files".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"."}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });

    acc.process(&StreamEvent::ContentBlockStart {
        index: 1,
        content_type: StreamContentType::ToolUse {
            id: "tool2".to_string(),
            name: "read_file".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"file.txt"}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 1 });

    assert_eq!(acc.tool_uses.len(), 2);
    assert_eq!(acc.tool_uses[0].name, "list_files");
    assert_eq!(acc.tool_uses[1].name, "read_file");
}

#[test]
fn test_stream_accumulator_ping_and_error() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::Ping);
    acc.process(&StreamEvent::Error {
        message: "test error".to_string(),
        request_id: None,
    });

    assert!(acc.text_content.is_empty());
}

#[test]
fn test_stream_accumulator_captures_provider_request_id_from_http_meta() {
    // The synthetic `HttpMeta` preamble carries the HTTP-header
    // request id. The accumulator must adopt it so a subsequent
    // `MessageStart` (which carries only the Anthropic *message* id)
    // doesn't clobber the HTTP value.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::HttpMeta {
        request_id: Some("req_01XYZ".to_string()),
    });
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_01ABC".to_string(),
        model: "claude-sonnet-4".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    assert_eq!(acc.provider_request_id.as_deref(), Some("req_01XYZ"));
    assert_eq!(acc.message_id, "msg_01ABC");
}

#[test]
fn test_stream_accumulator_http_meta_with_none_is_noop() {
    // `SseStream` always emits the `HttpMeta` preamble, even when no
    // header was captured. A `None` value must not overwrite a later
    // body-level fallback.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::HttpMeta { request_id: None });
    acc.process(&StreamEvent::Error {
        message: "api_error: Internal server error".to_string(),
        request_id: Some("req_01FALLBACK".to_string()),
    });
    assert_eq!(
        acc.provider_request_id.as_deref(),
        Some("req_01FALLBACK"),
        "body-level request_id should be adopted when HTTP header was absent"
    );
}

#[test]
fn test_stream_accumulator_http_meta_wins_over_body_fallback() {
    // When both the header *and* the body carry a request_id, the
    // header value is authoritative (it covers the success path too
    // and is the one provider / router logs actually key on).
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::HttpMeta {
        request_id: Some("req_01HEADER".to_string()),
    });
    acc.process(&StreamEvent::Error {
        message: "api_error: Internal server error".to_string(),
        request_id: Some("req_01BODY".to_string()),
    });
    assert_eq!(acc.provider_request_id.as_deref(), Some("req_01HEADER"));
}

#[test]
fn test_stream_accumulator_into_response_propagates_error_with_no_content() {
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_01ABC".to_string(),
        model: "claude-sonnet-test".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::Error {
        message: "Internal server error".to_string(),
        request_id: None,
    });

    let err = acc.into_response(0, 100).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("stream terminated with error"),
        "expected stream-termination prefix, got: {msg}"
    );
    // Model and message_id should be threaded into the error so
    // operators can correlate with provider / router logs.
    assert!(
        msg.contains("model=claude-sonnet-test"),
        "missing model: {msg}"
    );
    assert!(msg.contains("msg_id=msg_01ABC"), "missing msg_id: {msg}");
    assert!(
        msg.contains("Internal server error"),
        "missing raw msg: {msg}"
    );
    // No HttpMeta was processed, so the error should NOT claim a
    // request_id — better to omit the key than to surface a
    // misleading `request_id=` fragment.
    assert!(
        !msg.contains("request_id="),
        "should not fabricate request_id when absent, got: {msg}"
    );
}

#[test]
fn test_stream_accumulator_into_response_error_includes_request_id() {
    // Golden for the F1 plumbing: when the transport layer captured a
    // response-header `x-request-id` via `HttpMeta`, the error string
    // must surface it alongside `model=…, msg_id=…` so the operator
    // can grep provider / router logs directly.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::HttpMeta {
        request_id: Some("req_01XYZ".to_string()),
    });
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_01ABC".to_string(),
        model: "claude-sonnet-4".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::Error {
        message: "api_error: Internal server error".to_string(),
        request_id: None,
    });

    let err = acc.into_response(0, 100).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stream terminated with error"), "got: {msg}");
    assert!(msg.contains("model=claude-sonnet-4"), "got: {msg}");
    assert!(msg.contains("msg_id=msg_01ABC"), "got: {msg}");
    assert!(msg.contains("request_id=req_01XYZ"), "got: {msg}");
    assert!(
        msg.contains("api_error: Internal server error"),
        "got: {msg}"
    );
}

#[test]
fn test_stream_accumulator_into_response_populates_trace_split_ids() {
    // Happy path: `into_response` must populate both `message_id` and
    // `provider_request_id` on `ProviderTrace` when the stream
    // finished cleanly, so downstream observers
    // (`emit_debug_llm_call`, persistence of `llm_calls.jsonl`) get
    // both ids.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::HttpMeta {
        request_id: Some("req_01HAPPY".to_string()),
    });
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_01HAPPY".to_string(),
        model: "claude-sonnet-4".to_string(),
        input_tokens: Some(10),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::MessageStop);
    let response = acc.into_response(10, 42).expect("happy path");
    assert_eq!(response.trace.message_id.as_deref(), Some("msg_01HAPPY"));
    assert_eq!(
        response.trace.provider_request_id.as_deref(),
        Some("req_01HAPPY")
    );
    assert_eq!(response.trace.model, "claude-sonnet-4");
}

#[test]
fn test_stream_accumulator_into_response_propagates_error_even_with_partial_content() {
    // Regression: previously `into_response` silently dropped a
    // mid-stream error if any text or tool_use content had been
    // accumulated. That caused partial tool_use blocks to be executed
    // as if the stream had finished cleanly. The fix always
    // propagates `stream_error`.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_partial".to_string(),
        model: "claude-sonnet-test".to_string(),
        input_tokens: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Partial response before upstream blew up".to_string(),
    });
    acc.process(&StreamEvent::Error {
        message: "overloaded_error: service is overloaded".to_string(),
        request_id: None,
    });

    let err = acc.into_response(0, 100).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("stream terminated with error"),
        "partial-content path must still fail, got: {msg}"
    );
    assert!(
        msg.contains("overloaded_error"),
        "error type preserved: {msg}"
    );
}

#[test]
fn test_stream_accumulator_into_response_error_without_message_start() {
    // If the SSE error arrives before `message_start` we still fail
    // cleanly and just omit the context fragment (no trailing empty
    // parentheses in the rendered reason). With no in-flight tool,
    // the variant is `ReasonerError::Transient` (status 502) -- so
    // the surface error message is wrapped with the standard API-error
    // prefix from the `Display` impl. The inner `reason` must still
    // omit the empty parens.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::Error {
        message: "connection reset by peer".to_string(),
        request_id: None,
    });

    let err = acc.into_response(0, 100).unwrap_err();
    match err {
        crate::ReasonerError::Transient {
            status, message, ..
        } => {
            assert_eq!(status, 502);
            assert_eq!(
                message, "stream terminated with error: connection reset by peer",
                "no empty context parentheses when metadata unavailable"
            );
        }
        other => panic!("expected Transient, got: {other:?}"),
    }
}

#[test]
fn test_provider_trace() {
    // Legacy `with_request_id` now writes to `message_id` (the
    // deprecation doc-comment explains the split). New code should
    // prefer `with_message_id` / `with_provider_request_id`.
    #[allow(deprecated)]
    let trace = ProviderTrace::new("claude", 500).with_request_id("req123");

    assert_eq!(trace.model, "claude");
    assert_eq!(trace.latency_ms, 500);
    assert_eq!(trace.message_id.as_deref(), Some("req123"));
    assert_eq!(trace.provider_request_id, None);
    // Legacy accessor falls back to `message_id` when no HTTP id is
    // populated, preserving behaviour for callers still reading
    // `trace.request_id()`.
    assert_eq!(trace.request_id().as_deref(), Some("req123"));
}

#[test]
fn test_provider_trace_new_split_ids() {
    // When both ids are present, `request_id()` prefers the HTTP one
    // (that's the key operators need for provider / router logs).
    let trace = ProviderTrace::new("claude", 100)
        .with_message_id("msg_01ABC")
        .with_provider_request_id("req_01XYZ");
    assert_eq!(trace.message_id.as_deref(), Some("msg_01ABC"));
    assert_eq!(trace.provider_request_id.as_deref(), Some("req_01XYZ"));
    assert_eq!(trace.request_id().as_deref(), Some("req_01XYZ"));
}

#[test]
fn test_provider_trace_accepts_legacy_request_id_alias() {
    // Old persisted bundles (`llm_calls.jsonl` entries written before
    // the split) stored the Anthropic message id under `request_id`.
    // The serde alias on `message_id` keeps those bundles loadable
    // — crucial for replay-based regression tests.
    let json = r#"{"request_id":"msg_01OLD","latency_ms":42,"model":"claude-sonnet-4"}"#;
    let trace: ProviderTrace = serde_json::from_str(json).expect("legacy shape deserializes");
    assert_eq!(trace.message_id.as_deref(), Some("msg_01OLD"));
    assert_eq!(trace.provider_request_id, None);
    assert_eq!(trace.model, "claude-sonnet-4");
}

#[test]
fn test_usage_with_cache_tokens() {
    let usage = Usage::new(100, 50).with_cache(Some(80), Some(20));
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cache_creation_input_tokens, Some(80));
    assert_eq!(usage.cache_read_input_tokens, Some(20));
    assert_eq!(usage.total(), 150);
}

#[test]
fn test_usage_default_has_no_cache() {
    let usage = Usage::default();
    assert_eq!(usage.cache_creation_input_tokens, None);
    assert_eq!(usage.cache_read_input_tokens, None);
}

#[test]
fn test_stream_accumulator_input_tokens_from_message_start() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_cache".to_string(),
        model: "claude".to_string(),
        input_tokens: Some(200),
        cache_creation_input_tokens: Some(150),
        cache_read_input_tokens: Some(50),
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Cached!".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });
    acc.process(&StreamEvent::MessageDelta {
        stop_reason: Some(StopReason::EndTurn),
        output_tokens: 3,
    });

    let response = acc.into_response(0, 100).unwrap();

    assert_eq!(response.usage.input_tokens, 200);
    assert_eq!(response.usage.cache_creation_input_tokens, Some(150));
    assert_eq!(response.usage.cache_read_input_tokens, Some(50));
    assert_eq!(response.model_used, "claude");
}

#[test]
fn test_stream_accumulator_interleaved_text_tool_text() {
    let mut acc = StreamAccumulator::new();

    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Text,
    });
    acc.process(&StreamEvent::TextDelta {
        text: "Before tool. ".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });

    acc.process(&StreamEvent::ContentBlockStart {
        index: 1,
        content_type: StreamContentType::ToolUse {
            id: "t1".to_string(),
            name: "read_file".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"a.txt"}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 1 });

    acc.process(&StreamEvent::ContentBlockStart {
        index: 2,
        content_type: StreamContentType::ToolUse {
            id: "t2".to_string(),
            name: "list_files".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"."}"#.to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 2 });

    assert_eq!(acc.text_content, "Before tool. ");
    assert_eq!(acc.tool_uses.len(), 2);
    assert_eq!(acc.tool_uses[0].id, "t1");
    assert_eq!(acc.tool_uses[1].id, "t2");
}

#[test]
fn test_stream_accumulator_no_events() {
    let acc = StreamAccumulator::new();
    assert!(acc.message_id.is_empty());
    assert!(acc.text_content.is_empty());
    assert!(acc.tool_uses.is_empty());
    assert!(acc.stop_reason.is_none());
    assert_eq!(acc.output_tokens, 0);
}

#[test]
fn test_stream_accumulator_finalize_uses_fallback_input_tokens() {
    let acc = StreamAccumulator::new();
    let response = acc.into_response(999, 50).unwrap();
    assert_eq!(response.usage.input_tokens, 999);
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert!(response.message.content.is_empty());
}

#[test]
fn test_stream_accumulator_signature_appends() {
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::Thinking,
    });
    acc.process(&StreamEvent::SignatureDelta {
        signature: "part1".to_string(),
    });
    acc.process(&StreamEvent::SignatureDelta {
        signature: "part2".to_string(),
    });
    acc.process(&StreamEvent::ContentBlockStop { index: 0 });
    assert_eq!(acc.thinking_signature, Some("part1part2".to_string()));
}

// ========================================================================
// ContentBlock — serialization round-trips
// ========================================================================

#[test]
fn test_content_block_thinking_serialization_round_trip() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".to_string(),
        signature: Some("sig".to_string()),
    };
    let json = serde_json::to_string(&block).unwrap();
    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    match parsed {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "hmm");
            assert_eq!(signature, Some("sig".to_string()));
        }
        _ => panic!("Expected Thinking"),
    }
}

#[test]
fn test_content_block_thinking_without_signature_skips_field() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".to_string(),
        signature: None,
    };
    let json = serde_json::to_string(&block).unwrap();
    assert!(!json.contains("signature"));
}

#[test]
fn test_content_block_tool_use_serialization_round_trip() {
    let block = ContentBlock::tool_use("id1", "run_command", serde_json::json!({"cmd": "ls"}));
    let json = serde_json::to_string(&block).unwrap();
    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    match parsed {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "id1");
            assert_eq!(name, "run_command");
            assert_eq!(input["cmd"], "ls");
        }
        _ => panic!("Expected ToolUse"),
    }
}

#[test]
fn test_content_block_tool_result_serialization() {
    let block =
        ContentBlock::tool_result("tu_1", ToolResultContent::text("file contents here"), false);
    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains("tool_result"));
    assert!(json.contains("tu_1"));
}

// ========================================================================
// ModelRequest builder — additional edge cases
// ========================================================================

#[test]
fn test_model_request_builder_with_thinking() {
    use super::request::ThinkingConfig;

    let request = ModelRequest::builder("model", "system")
        .thinking(ThinkingConfig {
            budget_tokens: 4096,
        })
        .try_build()
        .unwrap();

    assert!(request.thinking.is_some());
    assert_eq!(request.thinking.unwrap().budget_tokens, 4096);
}

#[test]
fn test_model_request_builder_with_auth_token() {
    let request = ModelRequest::builder("model", "system")
        .auth_token(Some("tok_abc".to_string()))
        .try_build()
        .unwrap();

    assert_eq!(request.auth_token, Some("tok_abc".to_string()));
}

#[test]
fn test_model_request_builder_with_upstream_provider_family() {
    let request = ModelRequest::builder("model", "system")
        .upstream_provider_family(Some("deepseek".to_string()))
        .try_build()
        .unwrap();

    assert_eq!(
        request.upstream_provider_family,
        Some("deepseek".to_string())
    );
}

#[test]
fn test_model_request_builder_multiple_messages() {
    let request = ModelRequest::builder("model", "system")
        .message(Message::user("first"))
        .message(Message::assistant("response"))
        .message(Message::user("second"))
        .try_build()
        .unwrap();

    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(request.messages[1].role, Role::Assistant);
    assert_eq!(request.messages[2].role, Role::User);
}

// ========================================================================
// StopReason serialization
// ========================================================================

#[test]
fn test_stop_reason_serialization() {
    let reasons = [
        (StopReason::EndTurn, "\"end_turn\""),
        (StopReason::ToolUse, "\"tool_use\""),
        (StopReason::MaxTokens, "\"max_tokens\""),
        (StopReason::StopSequence, "\"stop_sequence\""),
    ];
    for (reason, expected) in reasons {
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, expected);
        let parsed: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }
}

// ========================================================================
// Role serialization
// ========================================================================

#[test]
fn test_role_serialization() {
    assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
    assert_eq!(
        serde_json::to_string(&Role::Assistant).unwrap(),
        "\"assistant\""
    );
}

// ========================================================================
// StreamAbortedWithPartial tests (per-tool-call streaming retry)
// ========================================================================

#[test]
fn stream_aborted_with_partial_preserves_tool_use() {
    // Mid-stream SSE error landing AFTER a content_block_start +
    // partial input_json_delta MUST surface the in-flight tool_use
    // inside `StreamAbortedWithPartial`, not get silently promoted
    // into `tool_uses` and treated like a successful call. This is
    // the contract `complete_with_streaming` relies on to drive its
    // per-tool-call retry loop.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_abort".to_string(),
        model: "claude-sonnet".to_string(),
        input_tokens: Some(5),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::ContentBlockStart {
        index: 0,
        content_type: StreamContentType::ToolUse {
            id: "toolu_abc".to_string(),
            name: "write_file".to_string(),
        },
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"{"path":"src/"#.to_string(),
    });
    acc.process(&StreamEvent::InputJsonDelta {
        partial_json: r#"lib.rs","#.to_string(),
    });
    // No ContentBlockStop -- the stream dies here.
    acc.process(&StreamEvent::Error {
        message: "overloaded_error: provider overloaded".to_string(),
        request_id: None,
    });

    let err = acc
        .into_response(0, 100)
        .expect_err("aborted stream must be an error");

    match err {
        crate::ReasonerError::StreamAbortedWithPartial {
            reason,
            partial_tool_use,
        } => {
            assert!(
                reason.contains("overloaded_error"),
                "reason should carry upstream message, got: {reason}"
            );
            let partial = partial_tool_use.expect("partial_tool_use must be Some");
            assert_eq!(partial.tool_use_id, "toolu_abc");
            assert_eq!(partial.tool_name, "write_file");
            assert_eq!(partial.partial_json, "{\"path\":\"src/lib.rs\",");
        }
        other => panic!("expected StreamAbortedWithPartial, got: {other:?}"),
    }
}

#[test]
fn stream_aborted_without_tool_use_returns_transient() {
    // When the SSE error arrives before any `tool_use` content-block
    // started, the no-partial branch must surface as a plain
    // `ReasonerError::Transient` (status 502) so the agent loop's
    // `stream_error_is_retryable` classifier routes the failure
    // through `complete_and_emit_as_deltas` (which has tracing + a
    // proper non-streaming fallback) instead of the silent
    // per-tool-call streaming retry loop in
    // `retry_streaming_for_partial_tool_use`. See the
    // `fix-silent-stream-retry-storm` plan for why this routing
    // matters: without it, a single Anthropic 5xx blip with no tool
    // in flight wedges chat in an invisible 28-second silent-retry
    // storm.
    let mut acc = StreamAccumulator::new();
    acc.process(&StreamEvent::MessageStart {
        message_id: "msg_abort2".to_string(),
        model: "claude-sonnet".to_string(),
        input_tokens: Some(5),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    acc.process(&StreamEvent::Error {
        message: "api_error: internal server error".to_string(),
        request_id: None,
    });

    let err = acc
        .into_response(0, 100)
        .expect_err("aborted stream must be an error");

    match err {
        crate::ReasonerError::Transient {
            status,
            message,
            retry_after,
        } => {
            assert_eq!(status, 502, "expected synthetic 502 for stream-error abort");
            assert!(
                message.contains("api_error"),
                "message should carry upstream error type, got: {message}"
            );
            assert!(
                message.contains("stream terminated with error"),
                "message should carry the canonical prefix, got: {message}"
            );
            assert!(retry_after.is_none());
        }
        other => panic!("expected Transient, got: {other:?}"),
    }
}
