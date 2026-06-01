//! WebSocket session protocol message types.
//!
//! Re-exports canonical types from `aura-protocol` and provides
//! harness-specific helpers that depend on aura-runtime-internal
//! types (e.g. [`aura_model_reasoner::ToolDefinition`]).
//!
//! Phase A note: the wire→core conversion helpers
//! (`installed_tool_to_core`, `installed_integration_to_core`,
//! `agent_tool_permissions_from_wire`, `tool_state_from_wire`,
//! `tool_state_to_wire`) moved to
//! [`aura_protocol::conversions`] so the protocol crate is the
//! single canonical wire↔core seam. The re-exports below preserve
//! the in-crate import path so callers do not have to chase the
//! move.

pub use aura_protocol::*;

use aura_core_types::ToolState;
use aura_model_reasoner::ToolDefinition;

/// Convert a reasoner [`ToolDefinition`] into a protocol [`ToolInfo`]
/// with an explicit tri-state permission annotation.
pub fn tool_info_from_definition_with_state(
    td: &ToolDefinition,
    effective_state: ToolState,
) -> ToolInfo {
    ToolInfo {
        name: td.name.clone(),
        description: td.description.clone(),
        effective_state: aura_protocol::tool_state_to_wire(effective_state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Inbound message deserialization
    // ========================================================================

    #[test]
    fn test_inbound_user_message() {
        let json = serde_json::json!({"type": "user_message", "content": "hello world"});
        let msg: InboundMessage = serde_json::from_value(json).unwrap();
        match msg {
            InboundMessage::UserMessage(um) => assert_eq!(um.content, "hello world"),
            _ => panic!("Expected UserMessage"),
        }
    }

    #[test]
    fn test_inbound_cancel() {
        let json = serde_json::json!({"type": "cancel"});
        let msg: InboundMessage = serde_json::from_value(json).unwrap();
        assert!(matches!(msg, InboundMessage::Cancel));
    }

    #[test]
    fn test_inbound_approval_response_approved() {
        let json = serde_json::json!({
            "type": "approval_response",
            "tool_use_id": "tu_123",
            "approved": true
        });
        let msg: InboundMessage = serde_json::from_value(json).unwrap();
        match msg {
            InboundMessage::ApprovalResponse(ar) => {
                assert_eq!(ar.tool_use_id, "tu_123");
                assert!(ar.approved);
            }
            _ => panic!("Expected ApprovalResponse"),
        }
    }

    #[test]
    fn test_inbound_approval_response_denied() {
        let json = serde_json::json!({
            "type": "approval_response",
            "tool_use_id": "tu_456",
            "approved": false
        });
        let msg: InboundMessage = serde_json::from_value(json).unwrap();
        match msg {
            InboundMessage::ApprovalResponse(ar) => {
                assert_eq!(ar.tool_use_id, "tu_456");
                assert!(!ar.approved);
            }
            _ => panic!("Expected ApprovalResponse"),
        }
    }

    #[test]
    fn test_inbound_unknown_type_fails() {
        let json = serde_json::json!({"type": "nonexistent"});
        assert!(serde_json::from_value::<InboundMessage>(json).is_err());
    }

    #[test]
    fn test_inbound_missing_type_fails() {
        let json = serde_json::json!({"content": "hello"});
        assert!(serde_json::from_value::<InboundMessage>(json).is_err());
    }

    // ========================================================================
    // Outbound message serialization
    // ========================================================================

    #[test]
    fn test_outbound_session_ready_roundtrip() {
        let msg = OutboundMessage::SessionReady(SessionReady {
            session_id: "sess_1".to_string(),
            tools: vec![
                ToolInfo {
                    name: "read_file".to_string(),
                    description: "Read a file".to_string(),
                    effective_state: ToolStateWire::On,
                },
                ToolInfo {
                    name: "write_file".to_string(),
                    description: "Write a file".to_string(),
                    effective_state: ToolStateWire::On,
                },
            ],
            skills: vec![],
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "session_ready");
        assert_eq!(json["session_id"], "sess_1");
        assert_eq!(json["tools"].as_array().unwrap().len(), 2);
        assert_eq!(json["tools"][0]["name"], "read_file");
    }

    #[test]
    fn test_outbound_assistant_message_start() {
        let msg = OutboundMessage::AssistantMessageStart(AssistantMessageStart {
            message_id: "msg_1".to_string(),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "assistant_message_start");
        assert_eq!(json["message_id"], "msg_1");
    }

    #[test]
    fn test_outbound_text_delta() {
        let msg = OutboundMessage::TextDelta(TextDelta {
            text: "Hello, ".to_string(),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["text"], "Hello, ");
    }

    #[test]
    fn test_outbound_thinking_delta() {
        let msg = OutboundMessage::ThinkingDelta(ThinkingDelta {
            thinking: "Let me consider...".to_string(),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "thinking_delta");
        assert_eq!(json["thinking"], "Let me consider...");
    }

    #[test]
    fn test_outbound_tool_use_start() {
        let msg = OutboundMessage::ToolUseStart(ToolUseStart {
            id: "tu_1".to_string(),
            name: "read_file".to_string(),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tool_use_start");
        assert_eq!(json["id"], "tu_1");
        assert_eq!(json["name"], "read_file");
    }

    #[test]
    fn test_outbound_tool_result() {
        let msg = OutboundMessage::ToolResult(ToolResultMsg {
            name: "read_file".to_string(),
            result: "file contents here".to_string(),
            is_error: false,
            tool_use_id: Some("tu_1".to_string()),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["name"], "read_file");
        assert!(!json["is_error"].as_bool().unwrap());
        assert_eq!(json["tool_use_id"], "tu_1");
    }

    #[test]
    fn test_outbound_tool_result_error() {
        let msg = OutboundMessage::ToolResult(ToolResultMsg {
            name: "write_file".to_string(),
            result: "permission denied".to_string(),
            is_error: true,
            tool_use_id: None,
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json["is_error"].as_bool().unwrap());
        assert_eq!(json["result"], "permission denied");
        assert!(json.get("tool_use_id").is_none());
    }

    #[test]
    fn test_outbound_assistant_message_end() {
        let msg = OutboundMessage::AssistantMessageEnd(Box::new(AssistantMessageEnd {
            message_id: "msg_1".to_string(),
            stop_reason: "end_turn".to_string(),
            usage: SessionUsage {
                input_tokens: 100,
                output_tokens: 50,
                estimated_context_tokens: 150,
                cache_creation_input_tokens: 25,
                cache_read_input_tokens: 10,
                cumulative_input_tokens: 200,
                cumulative_output_tokens: 100,
                cumulative_cache_creation_input_tokens: 50,
                cumulative_cache_read_input_tokens: 20,
                context_utilization: 0.5,
                model: "claude-opus-4-7".to_string(),
                provider: "anthropic".to_string(),
                context_breakdown: ContextBreakdown {
                    system_prompt_tokens: 7,
                    tools_tokens: 11,
                    skills_tokens: 13,
                    mcp_tokens: 0,
                    subagents_tokens: 17,
                    conversation_tokens: 102,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                context_contents: None,
            },
            files_changed: FilesChanged {
                created: vec!["new.txt".to_string()],
                modified: vec!["old.txt".to_string()],
                deleted: vec![],
                diffs: vec![],
            },
            originating_user_id: None,
        }));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "assistant_message_end");
        assert_eq!(json["message_id"], "msg_1");
        assert_eq!(json["stop_reason"], "end_turn");
        assert_eq!(json["usage"]["input_tokens"], 100);
        assert_eq!(json["usage"]["output_tokens"], 50);
    }

    #[test]
    fn test_outbound_error_msg() {
        let msg = OutboundMessage::Error(ErrorMsg {
            code: "rate_limit".to_string(),
            message: "Too many requests".to_string(),
            recoverable: true,
            support_id: None,
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["code"], "rate_limit");
        assert!(json["recoverable"].as_bool().unwrap());
        assert!(json.get("support_id").is_none());
    }

    #[test]
    fn test_outbound_error_msg_with_support_id_roundtrips() {
        let msg = OutboundMessage::Error(ErrorMsg {
            code: "agent_stalled".to_string(),
            message: "Agent loop made no forward progress for 3 iterations".to_string(),
            recoverable: true,
            support_id: Some("0123456789ab".to_string()),
        });
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["support_id"], "0123456789ab");

        let back: OutboundMessage = serde_json::from_value(json).unwrap();
        match back {
            OutboundMessage::Error(err) => {
                assert_eq!(err.support_id.as_deref(), Some("0123456789ab"));
                assert!(err.recoverable);
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn test_tool_info_from_tool_definition() {
        let td = ToolDefinition::new(
            "test_tool",
            "A test tool",
            serde_json::json!({"type": "object"}),
        );
        let info = tool_info_from_definition_with_state(&td, ToolState::Allow);
        assert_eq!(info.name, "test_tool");
        assert_eq!(info.description, "A test tool");
        assert_eq!(info.effective_state, ToolStateWire::On);
    }
}
