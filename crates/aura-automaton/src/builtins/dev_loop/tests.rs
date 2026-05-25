use aura_agent::agent_runner::TaskExecutionResult;
use aura_reasoner::{ContentBlock, Message, Role, ToolResultContent};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    commit_and_push, forward_agent_event, validate_execution, TaskAggregate,
    COMMIT_SKIPPED_NO_CHANGES,
};
use crate::context::TickContext;
use crate::events::AutomatonEvent;
use crate::state::AutomatonState;
use crate::types::AutomatonId;

#[test]
fn forwards_text_delta_with_task_id() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("hello".to_string()),
        Some("task-1"),
    );

    let event = rx.try_recv().expect("expected forwarded text delta");
    match event {
        AutomatonEvent::TextDelta { task_id, text } => {
            assert_eq!(task_id.as_deref(), Some("task-1"));
            assert_eq!(text, "hello");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn forwards_chat_text_delta_without_task_id() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("hello".to_string()),
        None,
    );

    let event = rx.try_recv().expect("expected forwarded text delta");
    match event {
        AutomatonEvent::TextDelta { task_id, text } => {
            assert!(task_id.is_none());
            assert_eq!(text, "hello");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn forwards_tool_start_with_task_id() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ToolStart {
            id: "tool-1".to_string(),
            name: "run_command".to_string(),
        },
        Some("task-1"),
    );

    let event = rx.try_recv().expect("expected forwarded tool start");
    match event {
        AutomatonEvent::ToolCallStarted { task_id, id, name } => {
            assert_eq!(task_id.as_deref(), Some("task-1"));
            assert_eq!(id, "tool-1");
            assert_eq!(name, "run_command");
            let wire = serde_json::to_value(AutomatonEvent::ToolCallStarted { task_id, id, name })
                .expect("serialize tool start");
            assert_eq!(wire["type"], "tool_use_start");
            assert_eq!(wire["task_id"], "task-1");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn forwards_valid_tool_input_snapshot() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ToolInputSnapshot {
            id: "tool-1".to_string(),
            name: "run_command".to_string(),
            input: r#"{"command":"npm run build"}"#.to_string(),
        },
        Some("task-1"),
    );

    let event = rx.try_recv().expect("expected forwarded event");
    match event {
        AutomatonEvent::ToolCallSnapshot {
            task_id,
            id,
            name,
            input,
            snapshot_partial,
        } => {
            assert_eq!(task_id.as_deref(), Some("task-1"));
            assert_eq!(id, "tool-1");
            assert_eq!(name, "run_command");
            assert_eq!(input["command"], "npm run build");
            assert!(
                !snapshot_partial,
                "parseable JSON must surface as a complete snapshot"
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn forwards_partial_tool_input_snapshot_with_flag() {
    // Partial JSON streamed mid-`tool_use` must now be forwarded with
    // `snapshot_partial: true` so the UI can render a live
    // in-flight card instead of dropping the event and leaving the
    // tool preview blank while the model is still writing.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ToolInputSnapshot {
            id: "tool-1".to_string(),
            name: "write_file".to_string(),
            input: "{\"path\":\"src/".to_string(),
        },
        Some("task-1"),
    );

    let event = rx
        .try_recv()
        .expect("partial snapshot must still be forwarded");
    match event {
        AutomatonEvent::ToolCallSnapshot {
            task_id,
            id,
            name,
            input,
            snapshot_partial,
        } => {
            assert_eq!(task_id.as_deref(), Some("task-1"));
            assert_eq!(id, "tool-1");
            assert_eq!(name, "write_file");
            assert!(
                snapshot_partial,
                "unparseable JSON must set snapshot_partial"
            );
            // The raw string is preserved so consumers can run their
            // own partial-parser on it (or render as-is).
            assert_eq!(
                input,
                serde_json::Value::String("{\"path\":\"src/".to_string())
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn forwards_tool_call_retrying_event() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ToolCallRetrying {
            tool_use_id: "toolu_1".to_string(),
            tool_name: "write_file".to_string(),
            attempt: 2,
            max_attempts: 8,
            delay_ms: 500,
            reason: "overloaded_error".to_string(),
        },
        Some("task-1"),
    );

    let event = rx.try_recv().expect("ToolCallRetrying must forward");
    match event {
        AutomatonEvent::ToolCallRetrying {
            task_id,
            tool_use_id,
            tool_name,
            attempt,
            max_attempts,
            delay_ms,
            reason,
        } => {
            assert_eq!(task_id, "task-1");
            assert_eq!(tool_use_id, "toolu_1");
            assert_eq!(tool_name, "write_file");
            assert_eq!(attempt, 2);
            assert_eq!(max_attempts, 8);
            assert_eq!(delay_ms, 500);
            assert_eq!(reason, "overloaded_error");
        }
        other => panic!("expected ToolCallRetrying, got: {other:?}"),
    }
}

#[test]
fn forwards_tool_call_failed_event() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ToolCallFailed {
            tool_use_id: "toolu_1".to_string(),
            tool_name: "write_file".to_string(),
            reason: "retries exhausted".to_string(),
        },
        Some("task-1"),
    );

    let event = rx.try_recv().expect("ToolCallFailed must forward");
    match event {
        AutomatonEvent::ToolCallFailed {
            task_id,
            tool_use_id,
            tool_name,
            reason,
        } => {
            assert_eq!(task_id, "task-1");
            assert_eq!(tool_use_id, "toolu_1");
            assert_eq!(tool_name, "write_file");
            assert_eq!(reason, "retries exhausted");
        }
        other => panic!("expected ToolCallFailed, got: {other:?}"),
    }
}

/// Build an assistant message containing a single `tool_use` block.
fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
    }
}

/// Build a user message containing a single `tool_result` block.
fn user_tool_result(tool_use_id: &str, content: &str, is_error: bool) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: ToolResultContent::Text(content.to_string()),
            is_error,
        }],
    }
}

#[test]
fn validate_execution_is_identity_pass_with_file_ops() {
    // The post-hoc "no file ops" decomposition gate was removed; the
    // validator is now a thin identity helper. Smoke-test the
    // surviving call shape so future verdicts have a regression
    // anchor.
    let messages = vec![
        assistant_tool_use(
            "call_ok",
            "write_file",
            json!({ "path": "src/done.rs", "content": "ok" }),
        ),
        user_tool_result("call_ok", "wrote 2 bytes", false),
    ];
    let exec = TaskExecutionResult {
        reached_implementing: true,
        messages,
        ..TaskExecutionResult::default()
    };

    let ok = validate_execution(exec).expect("validator is now an identity pass");
    assert!(ok.reached_implementing);
    assert_eq!(ok.messages.len(), 2);
}

#[test]
fn validate_execution_is_identity_pass_without_file_ops() {
    // The historical "no file ops + not reached_implementing"
    // failure path is gone — the validator must NOT reject this
    // shape any more (the orchestrator + retry policy police
    // forward progress instead).
    let exec = TaskExecutionResult {
        reached_implementing: false,
        no_changes_needed: false,
        messages: vec![],
        ..TaskExecutionResult::default()
    };

    let ok = validate_execution(exec).expect("validator must be Ok for any shape");
    assert!(!ok.reached_implementing);
    assert!(!ok.no_changes_needed);
}

#[test]
fn validate_execution_passes_through_when_no_changes_needed() {
    let exec = TaskExecutionResult {
        reached_implementing: true,
        no_changes_needed: true,
        ..TaskExecutionResult::default()
    };

    let ok = validate_execution(exec).expect("no_changes_needed must pass through");
    assert!(ok.no_changes_needed);
}

// ---------------------------------------------------------------------------
// Commit-skip DoD precheck (Section 2 of fix_4.6-class_failures plan).
//
// These tests exercise `TaskAggregate::from_exec` and `commit_and_push`'s
// early-skip branch to ensure a task that produced no file changes and
// no verification evidence never dispatches `git_commit` /
// `git_commit_push`. See `TaskAggregate`'s docs for the chosen signal.
// ---------------------------------------------------------------------------

fn make_ctx() -> (TickContext, mpsc::Receiver<AutomatonEvent>) {
    let (tx, rx) = mpsc::channel(64);
    let ctx = TickContext::new(
        AutomatonId::from_string("test-automaton"),
        AutomatonState::new(),
        tx,
        json!({}),
        None,
        CancellationToken::new(),
    );
    (ctx, rx)
}

fn drain(rx: &mut mpsc::Receiver<AutomatonEvent>) -> Vec<AutomatonEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[test]
fn task_aggregate_from_empty_exec_is_zero() {
    let agg = TaskAggregate::from_exec(&TaskExecutionResult::default());
    assert_eq!(agg.files_changed, 0);
    assert_eq!(agg.verification_steps, 0);
    assert!(agg.should_skip_commit());
}

#[test]
fn task_aggregate_counts_successful_write_file_tool_results() {
    // When the runner's `file_ops` is empty but the message log shows
    // a successful write_file tool_result, we should still treat the
    // task as having file changes. Guards against runners that only
    // populate `file_ops` in some code paths.
    let messages = vec![
        assistant_tool_use(
            "call-1",
            "write_file",
            json!({ "path": "src/foo.rs", "content": "pub fn foo() {}" }),
        ),
        user_tool_result("call-1", r#"{"bytes_written":16}"#, false),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert_eq!(agg.files_changed, 1);
    assert_eq!(agg.verification_steps, 0);
    assert!(!agg.should_skip_commit());
}

#[test]
fn task_aggregate_dedupes_repeat_writes_to_same_path() {
    // Two successful write_file tool_results targeting the same path
    // should count as a single file change; otherwise the count inflates
    // and the DoD precheck could silently pass even when only one file
    // was ever touched.
    let messages = vec![
        assistant_tool_use(
            "call-1",
            "write_file",
            json!({ "path": "src/foo.rs", "content": "v1" }),
        ),
        user_tool_result("call-1", "ok", false),
        assistant_tool_use(
            "call-2",
            "write_file",
            json!({ "path": "src/foo.rs", "content": "v2" }),
        ),
        user_tool_result("call-2", "ok", false),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert_eq!(agg.files_changed, 1);
}

#[test]
fn task_aggregate_ignores_errored_write_tool_results() {
    // tool_result with is_error=true must NOT count as a file change.
    let messages = vec![
        assistant_tool_use(
            "call-1",
            "write_file",
            json!({ "path": "src/foo.rs", "content": "x" }),
        ),
        user_tool_result("call-1", "permission denied", true),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert_eq!(agg.files_changed, 0);
    assert!(agg.should_skip_commit());
}

#[test]
fn task_aggregate_flags_chunk_guarded_write_without_followup() {
    // Regression for task_id=4079e975: the agent's `write_file` was
    // short-circuited by the chunk guard (8 KB content, 6 KB cap),
    // emitting an `is_error=true` tool_result prefixed with
    // `[CHUNK_GUARD]`. The agent then wrote ~2 KB via a follow-up
    // `write_file` but never finished the file with `edit_file`
    // chunks, yet still signalled `task_completed`. The safety net
    // scans for the marker, notices the chunk-guarded path was
    // never written again SUCCESSFULLY, and flags the task as
    // having pending oversized writes so `record_task_success`
    // routes it to the failure path.
    //
    // `write_file` attempt #2 reuses the SAME path, so the only
    // successful write is for that same path — and in this fixture
    // the second write is also errored, simulating the "never
    // recovered" case.
    let chunk_guard_msg = "[CHUNK_GUARD] `write_file` content of 8193 bytes exceeds cap";
    let messages = vec![
        assistant_tool_use(
            "call-1",
            "write_file",
            json!({
                "path": "zero-sdk/src/messaging/group/types.rs",
                "content": "x".repeat(8193),
            }),
        ),
        user_tool_result("call-1", chunk_guard_msg, true),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert!(
        agg.has_pending_oversized_writes(),
        "unresolved chunk-guard must flag the task as pending"
    );
    assert_eq!(
        agg.pending_oversized_writes,
        vec!["zero-sdk/src/messaging/group/types.rs".to_string()]
    );
}

#[test]
fn task_aggregate_clears_pending_when_chunk_guard_is_recovered() {
    // The same path that triggered the chunk guard gets a successful
    // follow-up write: the safety net must NOT block `task_completed`
    // because the file on disk now matches the agent's intent.
    let chunk_guard_msg = "[CHUNK_GUARD] `write_file` content of 8193 bytes exceeds cap";
    let path = "zero-sdk/src/messaging/group/types.rs";
    let messages = vec![
        assistant_tool_use(
            "call-1",
            "write_file",
            json!({ "path": path, "content": "x".repeat(8193) }),
        ),
        user_tool_result("call-1", chunk_guard_msg, true),
        assistant_tool_use(
            "call-2",
            "write_file",
            json!({ "path": path, "content": "pub struct Group;" }),
        ),
        user_tool_result("call-2", "ok", false),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert!(
        !agg.has_pending_oversized_writes(),
        "recovered chunk-guard path must clear pending set"
    );
    assert_eq!(agg.files_changed, 1);
}

#[test]
fn task_aggregate_counts_run_command_as_verification_evidence() {
    let messages = vec![
        assistant_tool_use("call-1", "run_command", json!({ "command": "cargo test" })),
        user_tool_result("call-1", "test result: ok. 42 passed", false),
    ];
    let exec = TaskExecutionResult {
        messages,
        ..TaskExecutionResult::default()
    };
    let agg = TaskAggregate::from_exec(&exec);
    assert_eq!(agg.files_changed, 0);
    assert_eq!(agg.verification_steps, 1);
    assert!(!agg.should_skip_commit());
}

#[tokio::test]
async fn commit_and_push_emits_commit_skipped_when_aggregate_is_empty() {
    // When the aggregate shows zero files_changed and zero
    // verification_steps, `commit_and_push` must emit `CommitSkipped`
    // WITHOUT consulting `tool_executor` (so `None` is fine) and
    // WITHOUT touching any workspace (so `workspace_root = None` is
    // fine). This guarantees the skip path is deterministic and
    // independent of whether the workspace happens to be a git repo.
    let (mut ctx, mut rx) = make_ctx();
    let aggregate = TaskAggregate::default();
    assert!(aggregate.should_skip_commit());

    commit_and_push(&mut ctx, None, "task-42", &aggregate)
        .await
        .expect("commit precheck emits skip event");

    let events = drain(&mut rx);
    assert_eq!(
        events.len(),
        1,
        "expected exactly one event, got {events:?}"
    );
    match &events[0] {
        AutomatonEvent::CommitSkipped { task_id, reason } => {
            assert_eq!(task_id, "task-42");
            assert_eq!(reason, COMMIT_SKIPPED_NO_CHANGES);
        }
        other => panic!("expected CommitSkipped, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_and_push_does_not_skip_when_aggregate_has_file_changes() {
    // When the aggregate carries at least one file change, the skip
    // precheck must NOT fire. We deliberately pass `workspace_root =
    // None` so the existing post-precheck path bails early with no
    // further events; the assertion is simply that no CommitSkipped
    // event was emitted, i.e. the precheck did not short-circuit.
    let (mut ctx, mut rx) = make_ctx();
    let aggregate = TaskAggregate {
        files_changed: 1,
        verification_steps: 0,
        ..Default::default()
    };
    assert!(!aggregate.should_skip_commit());

    commit_and_push(&mut ctx, None, "task-42", &aggregate)
        .await
        .expect("commit precheck with missing workspace succeeds");

    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AutomatonEvent::CommitSkipped { .. })),
        "did not expect CommitSkipped, got {events:?}"
    );
}

#[tokio::test]
async fn commit_and_push_does_not_skip_when_aggregate_has_verification_only() {
    // A task with zero file changes but at least one verification step
    // (e.g. a shell-task that only ran `cargo test`) should still fall
    // through to the existing commit path; the skip is only for the
    // "nothing happened" case.
    let (mut ctx, mut rx) = make_ctx();
    let aggregate = TaskAggregate {
        files_changed: 0,
        verification_steps: 1,
        ..Default::default()
    };
    assert!(!aggregate.should_skip_commit());

    commit_and_push(&mut ctx, None, "task-42", &aggregate)
        .await
        .expect("commit precheck with non-git workspace succeeds");

    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AutomatonEvent::CommitSkipped { .. })),
        "did not expect CommitSkipped, got {events:?}"
    );
}
