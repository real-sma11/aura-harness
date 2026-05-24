use super::*;
use crate::constants::DEFAULT_EXPLORATION_ALLOWANCE;

fn make_tool(name: &str, input: serde_json::Value) -> ToolCallInfo {
    ToolCallInfo {
        id: "test_id".to_string(),
        name: name.to_string(),
        input,
    }
}

#[test]
fn test_detect_blocked_writes_allows_first_write() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx).unwrap();
    assert!(!result.blocked);
}

#[test]
fn test_detect_blocked_writes_blocks_second_write() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.written_paths.insert("test.rs".to_string());
    let tool = make_tool("delete_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_writes(&tool, &ctx);
    assert!(
        result.is_none(),
        "delete_file should bypass the duplicate-write detector"
    );
}

#[test]
fn test_detect_blocked_write_failures_at_threshold() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.write_failures
        .insert("test.rs".to_string(), WRITE_FAILURE_BLOCK_THRESHOLD);
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_write_failures(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_blocked_commands_under_threshold() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.consecutive_cmd_failures = CMD_FAILURE_BLOCK_THRESHOLD - 1;
    let tool = make_tool("run_command", serde_json::json!({"command": "cargo build"}));
    let result = detect_blocked_commands(&tool, &ctx).unwrap();
    assert!(!result.blocked);
}

#[test]
fn test_detect_blocked_commands_at_threshold() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.consecutive_cmd_failures = CMD_FAILURE_BLOCK_THRESHOLD;
    let tool = make_tool("run_command", serde_json::json!({"command": "cargo build"}));
    let result = detect_blocked_commands(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_blocked_exploration_allows_under() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("read_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_exploration(&tool, &ctx).unwrap();
    assert!(!result.blocked);
}

#[test]
fn test_detect_blocked_exploration_when_exceeded() {
    // After the round-2 strip the hard block is unconditional — no
    // `mark_plan_submitted` setup needed.
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.exploration_count = DEFAULT_EXPLORATION_ALLOWANCE;
    let tool = make_tool("read_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_blocked_exploration(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

/// Round-2 strip: the read hard-block fires regardless of whether
/// `submit_plan` has been called. Round 1 deleted the plan write
/// gate, so blocking reads pre-plan no longer leaves the agent with
/// no legal next tool — `write_file` / `edit_file` / `delete_file`
/// are always available.
#[test]
fn test_detect_blocked_exploration_blocks_pre_plan_too() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.exploration_count = DEFAULT_EXPLORATION_ALLOWANCE;
    assert!(
        !ctx.plan_submitted,
        "plan_submitted must default to false; the detector must not depend on it"
    );
    for tool_name in ["read_file", "list_files", "find_files", "stat_file", "search_code"] {
        let tool = make_tool(tool_name, serde_json::json!({"path": "test.rs"}));
        let result = detect_blocked_exploration(&tool, &ctx).unwrap();
        assert!(
            result.blocked,
            "exploration via `{tool_name}` must hard-block at the budget regardless of \
             plan_submitted (round-2 strip)"
        );
    }
}

#[test]
fn test_mark_plan_submitted_is_idempotent() {
    // Subsequent calls must be no-ops so callers (the agent loop's
    // signal observer) don't have to guard against re-observation
    // across iterations.
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    assert!(!ctx.plan_submitted);
    ctx.mark_plan_submitted();
    assert!(ctx.plan_submitted);
    ctx.mark_plan_submitted();
    assert!(ctx.plan_submitted);
}

#[test]
fn test_decrement_cooldowns_reduces_and_removes() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    ctx.write_cooldowns.insert("a.rs".to_string(), 2);
    ctx.write_cooldowns.insert("b.rs".to_string(), 1);
    ctx.decrement_cooldowns();
    assert_eq!(ctx.write_cooldowns.get("a.rs"), Some(&1));
    assert!(!ctx.write_cooldowns.contains_key("b.rs"));
}

#[test]
fn test_detect_missing_args_blocks_write_file_without_path() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("edit_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
    assert!(result.recovery_message.unwrap().contains("edit_file(path="));
}

#[test]
fn test_detect_missing_args_blocks_delete_file_without_path() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("delete_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_allows_write_file_with_path() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("write_file", serde_json::json!({"path": "test.rs"}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_blocks_write_file_with_empty_path_string() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("edit_file", serde_json::json!({"path": "   \t"}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_blocks_read_file_with_empty_path() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("read_file", serde_json::json!({"path": ""}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_uses_last_read_path_as_hint() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("run_command", serde_json::json!({"command": "  "}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_allows_run_command_with_command() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("run_command", serde_json::json!({"command": "cargo build"}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_allows_run_command_with_program() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool(
        "run_command",
        serde_json::json!({"program": "cargo", "args": ["build"]}),
    );
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_allows_run_command_with_shell_script() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool(
        "run_command",
        serde_json::json!({"shell_script": "cargo build", "allow_shell": true}),
    );
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_detect_missing_args_blocks_read_file_without_path() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("read_file", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx).unwrap();
    assert!(result.blocked);
}

#[test]
fn test_detect_missing_args_skips_unrelated_tools() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let tool = make_tool("list_files", serde_json::json!({}));
    let result = detect_missing_required_args(&tool, &ctx);
    assert!(result.is_none());
}

#[test]
fn test_pathless_write_hint_prefers_last_read_then_written() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
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
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let read_guard = ReadGuardState::default();
    let tool = make_tool("delete_file", serde_json::json!({}));
    let result = detect_all_blocked(&tool, &ctx, &read_guard);
    assert!(result.blocked);
}

#[test]
fn test_detect_all_blocked_combines_all_detectors() {
    let ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let read_guard = ReadGuardState::default();
    let tool = make_tool("write_file", serde_json::json!({"path": "new.rs"}));
    let result = detect_all_blocked(&tool, &ctx, &read_guard);
    assert!(!result.blocked);
}

#[test]
fn test_on_write_success_resets_state() {
    let mut ctx = BlockingContext::new(DEFAULT_EXPLORATION_ALLOWANCE);
    let mut read_guard = ReadGuardState::default();
    read_guard.record_full_read("test.rs");
    ctx.write_failures.insert("test.rs".to_string(), 2);
    ctx.on_write_success("test.rs", &mut read_guard);
    assert!(ctx.written_paths.contains("test.rs"));
    assert!(!ctx.write_failures.contains_key("test.rs"));
    assert_eq!(ctx.exploration_allowance, DEFAULT_EXPLORATION_ALLOWANCE + 2);
    assert_eq!(read_guard.full_read_count("test.rs"), 0);
}

/// Pin the structural-blocker constants so future drift is intentional.
///
/// History: round 0 (`harness-dev-loop-efficiency`) raised the read
/// caps ~3x to give explore/edit cycles headroom against an open
/// `plan_submitted` gate. Round 2 (2026-05) tightened them again
/// because the round-1 strip ungated the read block — without the
/// gate the loose caps were hiding read-only loops rather than
/// breaking them.
///
/// `EMPTY_PATH_BLOCK_LIMIT` and `CONSECUTIVE_ERROR_ITERATIONS_LIMIT`
/// are deliberately kept tight as last-ditch wedge guards.
#[test]
fn relaxed_constants_are_consistent() {
    use crate::constants::{
        CMD_FAILURE_BLOCK_THRESHOLD, CONSECUTIVE_ERROR_ITERATIONS_LIMIT,
        DEFAULT_EXPLORATION_ALLOWANCE, EMPTY_PATH_BLOCK_LIMIT, EXPLORATION_WARNING_MILD_OFFSET,
        EXPLORATION_WARNING_STRONG_OFFSET, MAX_RANGE_READS_PER_FILE, MAX_READS_PER_FILE,
        STALL_STREAK_THRESHOLD, WRITE_COOLDOWN_ITERATIONS, WRITE_FAILURE_BLOCK_THRESHOLD,
        WRITE_FILE_CHUNK_BYTES, WRITE_FILE_HARD_MAX_BYTES,
    };

    assert_eq!(DEFAULT_EXPLORATION_ALLOWANCE, 20);
    assert_eq!(MAX_READS_PER_FILE, 3);
    assert_eq!(MAX_RANGE_READS_PER_FILE, 5);
    assert_eq!(WRITE_FILE_CHUNK_BYTES, 32_000);
    assert_eq!(WRITE_FILE_HARD_MAX_BYTES, 32_000);
    assert_eq!(WRITE_FAILURE_BLOCK_THRESHOLD, 6);
    assert_eq!(WRITE_COOLDOWN_ITERATIONS, 1);
    assert_eq!(CMD_FAILURE_BLOCK_THRESHOLD, 8);
    assert_eq!(STALL_STREAK_THRESHOLD, 5);
    assert_eq!(EXPLORATION_WARNING_MILD_OFFSET, 8);
    assert_eq!(EXPLORATION_WARNING_STRONG_OFFSET, 4);

    // These two stay tight on purpose — pathless writes and turns that
    // are 100% errors never recover by adding more iterations.
    assert_eq!(EMPTY_PATH_BLOCK_LIMIT, 3);
    assert_eq!(CONSECUTIVE_ERROR_ITERATIONS_LIMIT, 5);
}
