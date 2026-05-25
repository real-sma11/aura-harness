use super::*;

fn make_tool(name: &str, input: serde_json::Value) -> ToolCallInfo {
    ToolCallInfo {
        id: "test_id".to_string(),
        name: name.to_string(),
        input,
    }
}

#[test]
fn test_detect_blocked_writes_allows_first_write() {
    let ctx = BlockingContext::default();
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx).unwrap();
    assert!(!result.blocked);
}

#[test]
fn test_detect_blocked_writes_blocks_second_write() {
    let mut ctx = BlockingContext::default();
    ctx.written_paths.insert("test.rs".to_string());
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx).unwrap();
    assert!(result.blocked);
    let recovery = result.recovery_message.unwrap();
    assert!(recovery.contains("already wrote"));
    assert!(recovery.contains("AURA_ELIDED"));
}

#[test]
fn test_detect_blocked_writes_allows_edit_file_on_written_path() {
    let mut ctx = BlockingContext::default();
    ctx.written_paths.insert("test.rs".to_string());
    let tool = make_tool("edit_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx);
    assert!(
        result.is_none(),
        "edit_file should bypass the duplicate-write detector"
    );
}

#[test]
fn test_detect_blocked_writes_allows_delete_file_on_written_path() {
    let mut ctx = BlockingContext::default();
    ctx.written_paths.insert("test.rs".to_string());
    let tool = make_tool("delete_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx);
    assert!(
        result.is_none(),
        "delete_file should bypass the duplicate-write detector"
    );
}

#[test]
fn test_mark_plan_submitted_is_idempotent() {
    // Subsequent calls must be no-ops so callers (the agent loop's
    // signal observer) don't have to guard against re-observation
    // across iterations.
    let mut ctx = BlockingContext::default();
    assert!(!ctx.plan_submitted);
    ctx.mark_plan_submitted();
    assert!(ctx.plan_submitted);
    ctx.mark_plan_submitted();
    assert!(ctx.plan_submitted);
}

#[test]
fn test_decrement_cooldowns_reduces_and_removes() {
    let mut ctx = BlockingContext::default();
    ctx.write_cooldowns.insert("a.rs".to_string(), 2);
    ctx.write_cooldowns.insert("b.rs".to_string(), 1);
    ctx.decrement_cooldowns();
    assert_eq!(ctx.write_cooldowns.get("a.rs"), Some(&1));
    assert!(!ctx.write_cooldowns.contains_key("b.rs"));
}

#[test]
fn test_detect_missing_args_blocks_write_file_without_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("write_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
    let msg = result.recovery_message.unwrap();
    assert!(msg.contains("requires a non-empty `path`"));
    assert!(
        msg.contains("write_file(path="),
        "block message must include a concrete example"
    );
}

#[test]
fn test_detect_missing_args_blocks_edit_file_without_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("edit_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
    assert!(result.recovery_message.unwrap().contains("edit_file(path="));
}

#[test]
fn test_detect_missing_args_blocks_delete_file_without_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("delete_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_allows_write_file_with_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_blocks_write_file_with_empty_path_string() {
    let ctx = BlockingContext::default();
    let tool = make_tool("write_file", serde_json::json!({"path": ""}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
    assert!(result
        .recovery_message
        .as_deref()
        .unwrap()
        .contains("non-empty `path`"));
}

#[test]
fn test_detect_missing_args_blocks_edit_file_with_whitespace_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("edit_file", serde_json::json!({"path": "   \t"}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_blocks_read_file_with_empty_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("read_file", serde_json::json!({"path": ""}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_uses_last_read_path_as_hint() {
    let mut ctx = BlockingContext::default();
    ctx.on_read_path("crates/zero-identity/src/identity.rs");
    let tool = make_tool("edit_file", serde_json::json!({}));
    let msg = detect_missing_required_args(&tool, &ctx)
        .unwrap()
        .recovery_message
        .unwrap();
    assert!(
        msg.contains("crates/zero-identity/src/identity.rs"),
        "hint from last-read path should appear in example, got: {msg}"
    );
    assert!(msg.contains("Definition-of-Done gate"));
}

#[test]
fn test_detect_missing_args_blocks_run_command_without_command() {
    let ctx = BlockingContext::default();
    let tool = make_tool("run_command", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
    assert!(result
        .recovery_message
        .unwrap()
        .contains("requires executable input"));
}

#[test]
fn test_detect_missing_args_blocks_run_command_with_empty_command() {
    let ctx = BlockingContext::default();
    let tool = make_tool("run_command", serde_json::json!({"command": "  "}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_allows_run_command_with_command() {
    let ctx = BlockingContext::default();
    let tool = make_tool("run_command", serde_json::json!({"command": "cargo build"}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_allows_run_command_with_program() {
    let ctx = BlockingContext::default();
    let tool = make_tool(
        "run_command",
        serde_json::json!({"program": "cargo", "args": ["build"]}),
    );
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_allows_run_command_with_shell_script() {
    let ctx = BlockingContext::default();
    let tool = make_tool(
        "run_command",
        serde_json::json!({"shell_script": "cargo build", "allow_shell": true}),
    );
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_blocks_read_file_without_path() {
    let ctx = BlockingContext::default();
    let tool = make_tool("read_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_skips_unrelated_tools() {
    let ctx = BlockingContext::default();
    let tool = make_tool("list_files", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_pathless_write_hint_prefers_last_read_then_written() {
    let mut ctx = BlockingContext::default();
    assert!(ctx.pathless_write_hint().is_none());
    ctx.written_paths.insert("src/lib.rs".into());
    assert_eq!(ctx.pathless_write_hint(), Some("src/lib.rs"));
    ctx.on_read_path("src/main.rs");
    assert_eq!(
        ctx.pathless_write_hint(),
        Some("src/main.rs"),
        "last-read path must take precedence over written fallback"
    );
}

#[test]
fn test_detect_all_blocked_catches_empty_args_write() {
    let ctx = BlockingContext::default();
    let read_guard = ReadGuardState::default();
    let tool = make_tool("delete_file", serde_json::json!({}));
    let result = detect_all_blocked(&tool, &ctx, &read_guard);
    assert!(result.blocked);
}

#[test]
fn test_detect_all_blocked_combines_all_detectors() {
    let ctx = BlockingContext::default();
    let read_guard = ReadGuardState::default();
    let tool = make_tool("write_file", serde_json::json!({"path": "new.rs"}));
    let result = detect_all_blocked(&tool, &ctx, &read_guard);
    assert!(!result.blocked);
}

#[test]
fn test_on_write_success_resets_state() {
    let mut ctx = BlockingContext::default();
    let mut read_guard = ReadGuardState::default();
    read_guard.record_full_read("test.rs");
    ctx.write_failures.insert("test.rs".to_string(), 2);
    ctx.on_write_success("test.rs", &mut read_guard);
    assert!(ctx.written_paths.contains("test.rs"));
    assert!(!ctx.write_failures.contains_key("test.rs"));
    assert_eq!(read_guard.full_read_count("test.rs"), 0);
}

/// Pin the size-of-wire write-chunk constants so future drift is
/// intentional. These are wire-shape limits driven by the API's
/// tool-input size, not behavioral heuristics — they survived the
/// cook-loop-fix strip (2026-05) on purpose.
#[test]
fn write_chunk_constants_are_pinned() {
    use crate::constants::{WRITE_FILE_CHUNK_BYTES, WRITE_FILE_HARD_MAX_BYTES};

    assert_eq!(WRITE_FILE_CHUNK_BYTES, 32_000);
    assert_eq!(WRITE_FILE_HARD_MAX_BYTES, 32_000);
}
