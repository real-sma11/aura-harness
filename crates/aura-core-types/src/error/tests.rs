use super::*;

#[test]
fn test_error_display() {
    let err = AuraError::storage("test storage error");
    assert!(err.to_string().contains("storage error"));
    assert!(err.to_string().contains("test storage error"));
}

#[test]
fn test_storage_error() {
    let err = AuraError::storage("disk full");
    assert!(matches!(err, AuraError::Storage { message, source: None } if message == "disk full"));
}

#[test]
fn test_storage_error_with_source() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err = AuraError::storage_with_source("read failed", io_err);

    match err {
        AuraError::Storage { message, source } => {
            assert_eq!(message, "read failed");
            assert!(source.is_some());
        }
        _ => panic!("Expected Storage error"),
    }
}

#[test]
fn test_agent_not_found() {
    let agent_id = AgentId::new([42u8; 32]);
    let err = AuraError::AgentNotFound { agent_id };

    let display = err.to_string();
    assert!(display.contains("agent not found"));
}

#[test]
fn test_record_entry_not_found() {
    let agent_id = AgentId::new([1u8; 32]);
    let err = AuraError::RecordEntryNotFound { agent_id, seq: 42 };

    let display = err.to_string();
    assert!(display.contains("record entry not found"));
    assert!(display.contains("seq=42"));
}

#[test]
fn test_sequence_mismatch() {
    let err = AuraError::SequenceMismatch {
        expected: 10,
        actual: 5,
    };

    let display = err.to_string();
    assert!(display.contains("sequence mismatch"));
    assert!(display.contains("expected 10"));
    assert!(display.contains("got 5"));
}

#[test]
fn test_serialization_error() {
    let err = AuraError::serialization("invalid JSON");
    assert!(
        matches!(err, AuraError::Serialization { message, source: None } if message == "invalid JSON")
    );
}

#[test]
fn test_deserialization_error() {
    let err = AuraError::deserialization("missing field");
    assert!(
        matches!(err, AuraError::Deserialization { message, source: None } if message == "missing field")
    );
}

#[test]
fn test_policy_violation() {
    let err = AuraError::policy_violation("tool not allowed");

    let display = err.to_string();
    assert!(display.contains("policy violation"));
    assert!(display.contains("tool not allowed"));
}

#[test]
fn test_tool_not_allowed() {
    let err = AuraError::ToolNotAllowed {
        tool: "dangerous_tool".to_string(),
    };

    let display = err.to_string();
    assert!(display.contains("tool not allowed"));
    assert!(display.contains("dangerous_tool"));
}

#[test]
fn test_tool_execution_failed() {
    let err = AuraError::ToolExecutionFailed {
        tool: "read_file".to_string(),
        reason: "permission denied".to_string(),
    };

    let display = err.to_string();
    assert!(display.contains("tool execution failed"));
    assert!(display.contains("read_file"));
    assert!(display.contains("permission denied"));
}

#[test]
fn test_tool_timeout() {
    let err = AuraError::ToolTimeout {
        tool: "run_command".to_string(),
        timeout_ms: 30000,
    };

    let display = err.to_string();
    assert!(display.contains("tool timeout"));
    assert!(display.contains("30000"));
}

#[test]
fn test_sandbox_violation() {
    let err = AuraError::SandboxViolation {
        path: "../../../etc/passwd".to_string(),
    };

    let display = err.to_string();
    assert!(display.contains("sandbox violation"));
    assert!(display.contains("../../../etc/passwd"));
}

#[test]
fn test_reasoner_timeout() {
    let err = AuraError::ReasonerTimeout { timeout_ms: 60000 };

    let display = err.to_string();
    assert!(display.contains("reasoner timeout"));
    assert!(display.contains("60000"));
}

#[test]
fn test_invalid_transaction() {
    let err = AuraError::InvalidTransaction {
        reason: "empty payload".to_string(),
    };

    let display = err.to_string();
    assert!(display.contains("invalid transaction"));
    assert!(display.contains("empty payload"));
}

#[test]
fn test_invalid_action() {
    let action_id = ActionId::new([7u8; 16]);
    let err = AuraError::InvalidAction {
        action_id,
        reason: "malformed payload".to_string(),
    };

    let display = err.to_string();
    assert!(display.contains("invalid action"));
    assert!(display.contains("malformed payload"));
}

#[test]
fn test_from_serde_json_error() {
    let json_err = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
    let aura_err: AuraError = json_err.into();

    assert!(matches!(aura_err, AuraError::Deserialization { .. }));
}

#[test]
fn test_error_helper_functions() {
    assert!(matches!(
        AuraError::kernel("test"),
        AuraError::Kernel { .. }
    ));
    assert!(matches!(
        AuraError::executor("test"),
        AuraError::Executor { .. }
    ));
    assert!(matches!(
        AuraError::reasoner("test"),
        AuraError::Reasoner { .. }
    ));
    assert!(matches!(
        AuraError::validation("test"),
        AuraError::Validation { .. }
    ));
    assert!(matches!(
        AuraError::configuration("test"),
        AuraError::Configuration { .. }
    ));
    assert!(matches!(
        AuraError::internal("test"),
        AuraError::Internal { .. }
    ));
}

#[test]
fn test_result_type_alias() {
    fn returns_result() -> i32 {
        42
    }

    fn returns_error() -> Result<i32> {
        Err(AuraError::internal("test"))
    }

    assert_eq!(returns_result(), 42);
    assert!(returns_error().is_err());
}

#[test]
fn test_deserialization_error_with_source() {
    let io_err = std::io::Error::new(std::io::ErrorKind::InvalidData, "bad data");
    let err = AuraError::deserialization_with_source("parse failed", io_err);
    match err {
        AuraError::Deserialization { message, source } => {
            assert_eq!(message, "parse failed");
            assert!(source.is_some());
        }
        _ => panic!("Expected Deserialization error"),
    }
}

#[test]
fn test_serialization_with_source() {
    let io_err = std::io::Error::other("write failed");
    let err = AuraError::serialization_with_source("encode failed", io_err);
    match err {
        AuraError::Serialization { message, source } => {
            assert_eq!(message, "encode failed");
            assert!(source.is_some());
        }
        _ => panic!("Expected Serialization error"),
    }
}

#[test]
fn test_error_display_contains_message_for_all_variants() {
    let cases: Vec<(AuraError, &str)> = vec![
        (AuraError::storage("msg"), "storage error: msg"),
        (AuraError::kernel("msg"), "kernel error: msg"),
        (AuraError::executor("msg"), "executor error: msg"),
        (AuraError::reasoner("msg"), "reasoner error: msg"),
        (AuraError::validation("msg"), "validation error: msg"),
        (AuraError::configuration("msg"), "configuration error: msg"),
        (AuraError::internal("msg"), "internal error: msg"),
        (AuraError::serialization("msg"), "serialization error: msg"),
        (
            AuraError::deserialization("msg"),
            "deserialization error: msg",
        ),
        (AuraError::policy_violation("msg"), "policy violation: msg"),
    ];

    for (err, expected) in cases {
        assert_eq!(err.to_string(), expected);
    }
}

#[test]
fn test_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AuraError>();
}

#[test]
fn test_reasoner_unavailable() {
    let err = AuraError::ReasonerUnavailable {
        reason: "rate limited".to_string(),
    };
    let display = err.to_string();
    assert!(display.contains("reasoner unavailable"));
    assert!(display.contains("rate limited"));
}

#[test]
fn test_action_not_allowed() {
    let err = AuraError::ActionNotAllowed {
        action_kind: "Delegate".to_string(),
    };
    let display = err.to_string();
    assert!(display.contains("action not allowed"));
    assert!(display.contains("Delegate"));
}

#[test]
fn test_inbox_empty() {
    let agent_id = AgentId::new([0u8; 32]);
    let err = AuraError::InboxEmpty { agent_id };
    let display = err.to_string();
    assert!(display.contains("inbox empty"));
}

#[test]
fn test_error_debug_format() {
    let err = AuraError::internal("debug test");
    let debug_str = format!("{err:?}");
    assert!(debug_str.contains("Internal"));
    assert!(debug_str.contains("debug test"));
}

#[test]
fn test_from_serde_json_preserves_source() {
    let json_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
    let err_msg = json_err.to_string();
    let aura_err: AuraError = json_err.into();
    match aura_err {
        AuraError::Deserialization { message, source } => {
            assert_eq!(message, err_msg);
            assert!(source.is_some());
        }
        _ => panic!("Expected Deserialization error"),
    }
}
