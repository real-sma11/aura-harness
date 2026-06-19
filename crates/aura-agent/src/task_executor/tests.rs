use super::*;
use crate::agent_runner::TaskExecutionResult;
use crate::verify::TestSuiteOutcome;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

struct NoOpInner;

#[async_trait::async_trait]
impl AgentToolExecutor for NoOpInner {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "ok"))
            .collect()
    }
}

/// Test [`TaskTestRunner`] that returns a queue of pre-canned outcomes and
/// records every command it was invoked with.
///
/// Each call pops the next outcome; the queue is intentionally finite so a
/// runaway gate-loop in tests fails loudly with a panic instead of silently
/// reusing the last outcome. The `commands` log lets tests assert the gate
/// resolved the right test command (project config vs env override vs
/// inferred default).
#[derive(Debug, Default)]
struct MockTestRunner {
    queue: Mutex<Vec<anyhow::Result<TestSuiteOutcome>>>,
    calls: Mutex<u32>,
    commands: Mutex<Vec<String>>,
}

impl MockTestRunner {
    fn always_pass() -> Self {
        let mut q = Vec::new();
        for _ in 0..16 {
            q.push(Ok(TestSuiteOutcome {
                passed: true,
                summary: "10 passed, 0 failed".to_string(),
                ..Default::default()
            }));
        }
        Self {
            queue: Mutex::new(q),
            calls: Mutex::new(0),
            commands: Mutex::new(Vec::new()),
        }
    }

    fn always_fail() -> Self {
        let mut q = Vec::new();
        for _ in 0..12 {
            q.push(Ok(TestSuiteOutcome {
                passed: false,
                summary: "9 passed, 1 failed".to_string(),
                failed_tests: vec!["my_crate::tests::it_works".to_string()],
                raw_stderr: "thread 'it_works' panicked at 'assertion failed'".to_string(),
                ..Default::default()
            }));
        }
        Self {
            queue: Mutex::new(q),
            calls: Mutex::new(0),
            commands: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl TaskTestRunner for MockTestRunner {
    async fn run_tests(
        &self,
        _project_root: &std::path::Path,
        command: &str,
    ) -> anyhow::Result<TestSuiteOutcome> {
        *self.calls.lock().await += 1;
        self.commands.lock().await.push(command.to_string());
        let mut q = self.queue.lock().await;
        if q.is_empty() {
            anyhow::bail!("MockTestRunner queue exhausted");
        }
        q.remove(0)
    }
}

#[derive(Debug, Default)]
struct HangingTestRunner;

#[async_trait::async_trait]
impl TaskTestRunner for HangingTestRunner {
    async fn run_tests(
        &self,
        _project_root: &std::path::Path,
        _command: &str,
    ) -> anyhow::Result<TestSuiteOutcome> {
        std::future::pending::<anyhow::Result<TestSuiteOutcome>>().await
    }
}

fn make_executor_with_runner(runner: Arc<dyn TaskTestRunner>) -> TaskToolExecutor {
    TaskToolExecutor {
        inner: Arc::new(NoOpInner),
        project_folder: "/tmp/test".to_string(),
        build_command: None,
        test_command: Some("cargo test --workspace".to_string()),
        test_command_override: None,
        task_context: String::new(),
        tracked_file_ops: Default::default(),
        notes: Default::default(),
        follow_ups: Default::default(),
        stub_fix_attempts: Default::default(),
        test_runner: runner,
        task_phase: Arc::new(Mutex::new(TaskPhase::Implementing {
            plan: crate::planning::TaskPlan::empty(),
        })),
        self_review: Default::default(),
        event_tx: None,
        no_changes_needed: Default::default(),
        recent_tool_outcomes: Default::default(),
        reset_explore_on_phase_change: Arc::new(AtomicBool::new(false)),
    }
}

fn make_executor() -> TaskToolExecutor {
    make_executor_with_runner(Arc::new(MockTestRunner::always_pass()))
}

fn task_done_call(notes: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: "td_1".to_string(),
        name: "task_done".to_string(),
        input: serde_json::json!({ "notes": notes }),
    }
}

fn task_done_no_changes(notes: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: "td_1".to_string(),
        name: "task_done".to_string(),
        input: serde_json::json!({
            "notes": notes,
            "no_changes_needed": true,
        }),
    }
}

fn read_file_call(path: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: "read_1".to_string(),
        name: "read_file".to_string(),
        input: serde_json::json!({ "path": path }),
    }
}

// ------------------------------------------------------------------
// task_done guard tests
// ------------------------------------------------------------------

#[tokio::test]
async fn task_done_rejects_when_no_file_ops() {
    // After the 2026-05 strip, the rejection no longer distinguishes
    // exploring vs implementing — there is no write gate, so any
    // `task_done` without file ops gets the same "make some changes"
    // message regardless of phase.
    let executor = make_executor();
    let calls = [task_done_call("all done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(!results[0].stop_loop);
    let body = &results[0].content;
    assert!(
        body.contains("have not produced any file changes"),
        "expected file-ops rejection wording: {body}"
    );
    assert!(
        body.contains("no_changes_needed"),
        "rejection must keep the no_changes_needed escape hatch: {body}"
    );
}

#[tokio::test]
async fn task_done_no_writes_rejection_blocks_exploration_detours() {
    let executor = make_executor();
    let rejected = executor
        .execute(&[task_done_call("already implemented")])
        .await;
    assert!(rejected[0].is_error);

    let detour = executor.execute(&[read_file_call("src/lib.rs")]).await;
    assert_eq!(detour.len(), 1);
    assert!(detour[0].is_error);
    assert!(detour[0]
        .content
        .contains("task_done was just rejected because no file changes were produced"));
    assert!(detour[0].content.contains("no_changes_needed: true"));
}

#[tokio::test]
async fn task_done_no_changes_still_passes_after_no_write_rejection() {
    let executor = make_executor();
    let rejected = executor
        .execute(&[task_done_call("already implemented")])
        .await;
    assert!(rejected[0].is_error);

    let done = executor
        .execute(&[task_done_no_changes(
            "outbox CF is already implemented and wired",
        )])
        .await;
    assert_eq!(done.len(), 1);
    assert!(!done[0].is_error);
    assert!(done[0].stop_loop);
}

/// Parity check after the strip: an exploring-phase executor produces
/// the same rejection as an implementing-phase one. The submit_plan
/// gate is gone, so the wording must not regress to phase-specific
/// language.
#[tokio::test]
async fn task_done_in_exploring_phase_uses_phase_neutral_message() {
    let executor = make_exploring_executor();
    let calls = [task_done_call("done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(!results[0].stop_loop);
    let body = &results[0].content;
    assert!(
        !body.contains("EXPLORING phase"),
        "phase-aware rejection wording must be gone: {body}"
    );
    assert!(
        body.contains("have not produced any file changes"),
        "rejection must still tell the model to make file changes: {body}"
    );
}

#[tokio::test]
async fn task_done_succeeds_with_file_ops() {
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }

    let calls = [task_done_call("implemented feature")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(results[0].stop_loop);
    assert!(results[0].content.contains("completed"));
}

#[tokio::test]
async fn task_done_allows_no_ops_with_exemption() {
    let executor = make_executor();
    let calls = [task_done_no_changes(
        "analysis task, no code changes required",
    )];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(results[0].stop_loop);
    assert!(results[0].content.contains("completed"));
}

#[tokio::test]
async fn task_done_rejects_when_build_command_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut executor = make_executor();
    executor.project_folder = dir.path().to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        executor.build_command = Some("false".to_string());
    }
    #[cfg(windows)]
    {
        executor.build_command = Some("cmd /C exit 1".to_string());
    }
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }

    let results = executor.execute(&[task_done_call("done")]).await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_error,
        "task_done must be rejected when build fails: {}",
        results[0].content
    );
    assert!(
        !results[0].stop_loop,
        "rejected task_done must not stop the loop"
    );
    assert!(
        results[0].content.contains("Build check failed"),
        "rejection must include build failure context: {}",
        results[0].content
    );
}

// ------------------------------------------------------------------
// merge_into_result tests
// ------------------------------------------------------------------

#[tokio::test]
async fn merge_into_result_populates_notes_and_follow_ups() {
    let executor = make_executor();
    {
        let mut n = executor.notes.lock().await;
        *n = "executor notes".to_string();
    }
    {
        let mut fu = executor.follow_ups.lock().await;
        fu.push(FollowUpSuggestion {
            title: "next step".to_string(),
            description: "do more".to_string(),
        });
    }

    let mut result = TaskExecutionResult::default();
    executor.merge_into_result(&mut result).await;

    assert_eq!(result.notes, "executor notes");
    assert_eq!(result.follow_up_tasks.len(), 1);
    assert_eq!(result.follow_up_tasks[0].title, "next step");
}

#[tokio::test]
async fn merge_preserves_loop_notes_when_executor_notes_empty() {
    let executor = make_executor();
    let mut result = TaskExecutionResult {
        notes: "loop generated notes".to_string(),
        ..Default::default()
    };
    executor.merge_into_result(&mut result).await;

    assert_eq!(result.notes, "loop generated notes");
}

// ------------------------------------------------------------------
// pervasive error guard tests
// ------------------------------------------------------------------

#[tokio::test]
async fn task_done_rejects_when_last_command_failed() {
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        for _ in 0..4 {
            outcomes.record("read_file", false, "ok");
        }
        outcomes.record("run_command", true, "exit code 1\nerror: test failed");
    }
    let calls = [task_done_call("all done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(!results[0].stop_loop);
    assert!(results[0].content.contains("run_command"));
}

#[tokio::test]
async fn task_done_rejects_when_error_ratio_high() {
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        for _ in 0..2 {
            outcomes.record("read_file", false, "ok");
        }
        for _ in 0..8 {
            outcomes.record("read_file", true, "file not found");
        }
    }
    let calls = [task_done_call("done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(results[0].content.contains("failure rate"));
}

#[tokio::test]
async fn task_done_accepts_when_errors_low() {
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        for _ in 0..8 {
            outcomes.record("read_file", false, "ok");
        }
        for _ in 0..2 {
            outcomes.record("read_file", true, "file not found");
        }
    }
    let calls = [task_done_call("done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(results[0].stop_loop);
}

#[tokio::test]
async fn task_done_ignores_policy_denied_run_command() {
    // Task 2.4 regression: agent called `run_command` which was denied
    // by policy. `last_command_failed` must NOT be set because nothing
    // ran — there is no broken build to fix.
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        outcomes.record("run_command", true, "Tool 'run_command' is not allowed");
    }
    let calls = [task_done_call("all done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_error,
        "task_done should accept: {}",
        results[0].content
    );
    assert!(results[0].stop_loop);
}

#[tokio::test]
async fn policy_denials_do_not_count_against_error_ratio() {
    let executor = make_executor();
    {
        let mut ops = executor.tracked_file_ops.lock().await;
        ops.push(FileOp::Create {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        });
    }
    {
        let mut sr = executor.self_review.lock().await;
        sr.record_write("src/main.rs");
        sr.record_read("src/main.rs");
    }
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        // 9 policy denials + 1 real success -> ratio should be 0/10,
        // not 9/10. Without policy classification this would reject.
        for _ in 0..9 {
            outcomes.record("run_command", true, "Tool 'run_command' is not allowed");
        }
        outcomes.record("read_file", false, "ok");
    }
    let calls = [task_done_call("done")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_error,
        "should not be blocked by policy denials: {}",
        results[0].content
    );
}

/// Build an executor stuck in [`TaskPhase::Exploring`]. Used to exercise
/// the write/edit/delete gate without going through the real agent loop.
fn make_exploring_executor() -> TaskToolExecutor {
    TaskToolExecutor {
        inner: Arc::new(NoOpInner),
        project_folder: "/tmp/test".to_string(),
        build_command: None,
        test_command: Some("cargo test --workspace".to_string()),
        test_command_override: None,
        task_context: String::new(),
        tracked_file_ops: Default::default(),
        notes: Default::default(),
        follow_ups: Default::default(),
        stub_fix_attempts: Default::default(),
        test_runner: Arc::new(MockTestRunner::always_pass()),
        task_phase: Arc::new(Mutex::new(TaskPhase::Exploring)),
        self_review: Default::default(),
        event_tx: None,
        no_changes_needed: Default::default(),
        recent_tool_outcomes: Default::default(),
        reset_explore_on_phase_change: Arc::new(AtomicBool::new(false)),
    }
}

/// After the 2026-05 strip, the submit_plan write gate is gone:
/// `write_file` from the `Exploring` phase must reach the inner
/// executor (and succeed in this test since [`NoOpInner`] reports
/// every call as a no-op success).
#[tokio::test]
async fn write_file_in_exploring_phase_is_not_gated() {
    let executor = make_exploring_executor();
    let call = ToolCallInfo {
        id: "wf_1".into(),
        name: "write_file".into(),
        input: serde_json::json!({
            "path": "src/lib.rs",
            "content": "fn main() {}",
        }),
    };

    let results = executor.execute(&[call]).await;

    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(
        !r.is_error,
        "write_file must reach the delegate (gate removed): {}",
        r.content
    );
    let ops = executor.tracked_file_ops.lock().await;
    assert_eq!(ops.len(), 1, "write_file should record a file op");
}

#[tokio::test]
async fn submit_plan_resets_outcome_window() {
    let executor = TaskToolExecutor {
        inner: Arc::new(NoOpInner),
        project_folder: "/tmp/test".to_string(),
        build_command: None,
        test_command: Some("cargo test --workspace".to_string()),
        test_command_override: None,
        task_context: String::new(),
        tracked_file_ops: Default::default(),
        notes: Default::default(),
        follow_ups: Default::default(),
        stub_fix_attempts: Default::default(),
        test_runner: Arc::new(MockTestRunner::always_pass()),
        task_phase: Arc::new(Mutex::new(TaskPhase::Exploring)),
        self_review: Default::default(),
        event_tx: None,
        no_changes_needed: Default::default(),
        recent_tool_outcomes: Default::default(),
        reset_explore_on_phase_change: Arc::new(AtomicBool::new(false)),
    };
    // Simulate a noisy exploration phase: 10 errors accumulated.
    {
        let mut outcomes = executor.recent_tool_outcomes.lock().await;
        for _ in 0..10 {
            outcomes.record("read_file", true, "is not a file");
        }
        assert_eq!(outcomes.total(), 10);
    }

    // Submit a valid plan.
    let plan_call = ToolCallInfo {
        id: "sp_1".into(),
        name: "submit_plan".into(),
        input: serde_json::json!({
            "approach": "fix the bug by adding a null check that prevents the crash",
            "files_to_modify": ["src/main.rs"],
            "key_decisions": ["use an early return"],
        }),
    };
    let _ = executor.execute(&[plan_call]).await;

    // Outcome window must be cleared so the implementing phase starts
    // fresh.
    let outcomes = executor.recent_tool_outcomes.lock().await;
    assert_eq!(outcomes.total(), 0);
    assert_eq!(outcomes.real_errors(), 0);
    assert!(!outcomes.last_command_failed);
}

#[tokio::test]
async fn outcome_window_is_bounded() {
    let mut outcomes = RecentToolOutcomes::default();
    for _ in 0..100 {
        outcomes.record("read_file", true, "fail");
    }
    assert!(outcomes.total() <= RECENT_OUTCOMES_WINDOW);
    assert_eq!(outcomes.total(), RECENT_OUTCOMES_WINDOW);
}

// ------------------------------------------------------------------
// extract_notes_and_follow_ups tests
// ------------------------------------------------------------------

#[tokio::test]
async fn extract_parses_no_changes_needed_flag() {
    let executor = make_executor();
    let tc = task_done_no_changes("just an analysis");
    executor.extract_notes_and_follow_ups(&tc).await;

    assert!(*executor.no_changes_needed.lock().await);
    assert_eq!(*executor.notes.lock().await, "just an analysis");
}

// ------------------------------------------------------------------
// Codex parity: post-task_done best-effort test run tests
// ------------------------------------------------------------------

async fn seed_with_file_op(executor: &TaskToolExecutor) {
    let mut ops = executor.tracked_file_ops.lock().await;
    ops.push(FileOp::Create {
        path: "src/main.rs".to_string(),
        content: "fn main() {}".to_string(),
    });
    drop(ops);
    let mut sr = executor.self_review.lock().await;
    sr.record_write("src/main.rs");
    sr.record_read("src/main.rs");
}

async fn recv_test_warning(
    rx: &mut tokio::sync::mpsc::Receiver<AgentLoopEvent>,
) -> (bool, String, Vec<String>) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(event) = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("expected TestSuiteWarning event before timeout")
        else {
            panic!("event channel closed before TestSuiteWarning");
        };
        if let AgentLoopEvent::TestSuiteWarning {
            passed,
            summary,
            failed_tests,
        } = event
        {
            return (passed, summary, failed_tests);
        }
    }
}

#[tokio::test]
async fn task_done_emits_warning_when_tests_fail_but_does_not_retry() {
    // Codex parity replacement for the deleted retry/exhaustion suite:
    // a failing test suite is now a `TestSuiteWarning` event, not a
    // hard rejection. The runner must be invoked exactly once and the
    // `task_done` tool_result must succeed and stop the loop.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentLoopEvent>(8);
    let runner = Arc::new(MockTestRunner::always_fail());
    let mut executor = make_executor_with_runner(runner.clone());
    executor.event_tx = Some(tx);
    seed_with_file_op(&executor).await;

    let results = executor.execute(&[task_done_call("done")]).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_error,
        "task_done must succeed even when the test suite fails: {}",
        results[0].content
    );
    assert!(
        results[0].stop_loop,
        "successful task_done must stop the loop"
    );
    let (passed, summary, failed_tests) = recv_test_warning(&mut rx).await;
    assert_eq!(
        *runner.calls.lock().await,
        1,
        "the suite must run exactly once — no retry budget after Codex parity"
    );
    assert!(!passed, "warning must reflect the failing run");
    assert!(
        summary.contains("9 passed"),
        "warning summary must carry the runner outcome: {summary}"
    );
    assert_eq!(failed_tests, vec!["my_crate::tests::it_works".to_string()]);
}

#[tokio::test]
async fn task_done_does_not_hang_when_best_effort_test_run_stalls() {
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentLoopEvent>(8);
    let mut executor = make_executor_with_runner(Arc::new(HangingTestRunner));
    executor.event_tx = Some(tx);
    seed_with_file_op(&executor).await;

    let results = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        executor.execute(&[task_done_call("done")]),
    )
    .await
    .expect("advisory post-task_done tests must not hang completion");

    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(results[0].stop_loop);
}

#[test]
fn read_test_command_override_env_reflects_aura_config() {
    // Same defence-in-depth idea as for the disable flag: an operator
    // exporting `AURA_DOD_TEST_COMMAND=` to "clear" the override must
    // not get an empty string handed back as if it were a real
    // command. `aura_config` does that trimming once at startup; this
    // test pins the contract by overriding the installed value.
    fn install_override(value: Option<&str>) -> aura_config::ConfigGuard {
        let mut cfg = aura_config::current();
        cfg.agent.verify.test_command_override = value.map(str::to_string);
        aura_config::install_for_test(cfg)
    }

    {
        let _g = install_override(Some("cargo test --workspace"));
        assert_eq!(
            super::read_test_command_override_env(),
            Some("cargo test --workspace".to_string()),
            "non-empty override must round-trip"
        );
    }
    {
        let _g = install_override(None);
        assert_eq!(
            super::read_test_command_override_env(),
            None,
            "unset override must read as None"
        );
    }
}

#[tokio::test]
async fn resolve_test_command_prefers_env_override_over_project_config() {
    // The override field is captured at construction time, so this
    // test doesn't touch the global env — it exercises the resolution
    // priority directly. The env reader itself is covered above.
    let mut executor = make_executor();
    executor.test_command = Some("cargo test --workspace".to_string());
    executor.test_command_override = Some("pytest -q tests/smoke/".to_string());

    let (cmd, source) = executor
        .resolve_test_command(std::path::Path::new("/tmp/test"))
        .expect("override should resolve");
    assert_eq!(cmd, "pytest -q tests/smoke/");
    assert_eq!(source, "env override");
}

#[tokio::test]
async fn resolve_test_command_falls_back_to_project_config_when_no_override() {
    let mut executor = make_executor();
    executor.test_command = Some("npm test --silent".to_string());
    executor.test_command_override = None;

    let (cmd, source) = executor
        .resolve_test_command(std::path::Path::new("/tmp/test"))
        .expect("project config should resolve");
    assert_eq!(cmd, "npm test --silent");
    assert_eq!(source, "project config");
}

#[tokio::test]
async fn resolve_test_command_falls_back_to_inferred_default() {
    // Build a real on-disk Cargo project so the auto-detect path has
    // something to match. Using a temp dir keeps the test hermetic.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

    let mut executor = make_executor();
    executor.test_command = None;
    executor.test_command_override = None;

    let (cmd, source) = executor
        .resolve_test_command(dir.path())
        .expect("auto-detect should resolve from Cargo.toml");
    assert!(cmd.starts_with("cargo test"), "got {cmd}");
    assert_eq!(source, "manifest auto-detect");
}

// ------------------------------------------------------------------
// Phase-reset handshake tests
// ------------------------------------------------------------------

/// Build an executor stuck in [`TaskPhase::Exploring`] and wired to a
/// caller-supplied `Arc<AtomicBool>` so the test can inspect the
/// reset-signal flip directly.
fn make_exploring_executor_with_signal(signal: Arc<AtomicBool>) -> TaskToolExecutor {
    TaskToolExecutor {
        inner: Arc::new(NoOpInner),
        project_folder: "/tmp/test".to_string(),
        build_command: None,
        test_command: Some("cargo test --workspace".to_string()),
        test_command_override: None,
        task_context: String::new(),
        tracked_file_ops: Default::default(),
        notes: Default::default(),
        follow_ups: Default::default(),
        stub_fix_attempts: Default::default(),
        test_runner: Arc::new(MockTestRunner::always_pass()),
        task_phase: Arc::new(Mutex::new(TaskPhase::Exploring)),
        self_review: Default::default(),
        event_tx: None,
        no_changes_needed: Default::default(),
        recent_tool_outcomes: Default::default(),
        reset_explore_on_phase_change: signal,
    }
}

// ------------------------------------------------------------------
// Phase 1 contract — TaskToolExecutor accepts writes / task_done
// without ever requiring a `submit_plan` call (harness-v2).
// ------------------------------------------------------------------

/// `make_executor` already starts in `TaskPhase::Implementing`, but the
/// FIRST tool the executor sees in this test is a `write_file`. Pre-
/// Phase-1, that call was rejected from `Exploring` until `submit_plan`
/// flipped the phase. Phase 1 changed the production executor's initial
/// phase to `Implementing` (see `agent_runner::execute_task_tracked`),
/// so the very first interaction may be a write — pin that here at the
/// executor level so a future revert to a write-gated `Exploring`
/// default fails loudly.
#[tokio::test]
async fn task_tool_executor_accepts_write_file_as_first_interaction() {
    let executor = make_executor();
    let call = ToolCallInfo {
        id: "wf_first".into(),
        name: "write_file".into(),
        input: serde_json::json!({
            "path": "src/lib.rs",
            "content": "pub fn first() {}",
        }),
    };

    let results = executor.execute(&[call]).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_error,
        "first-iteration write_file must reach the inner executor (Phase 1 contract): {}",
        results[0].content,
    );
    let ops = executor.tracked_file_ops.lock().await;
    assert_eq!(
        ops.len(),
        1,
        "first-iteration write_file must record a tracked file op",
    );
    let phase = executor.task_phase.lock().await;
    assert!(
        matches!(*phase, TaskPhase::Implementing { .. }),
        "executor must remain in Implementing after a successful write \
         (Phase 1 contract: writes do not transition phase)",
    );
}

/// End-to-end Phase 1 pin: a fresh executor accepts `write_file`,
/// then a self-review `read_file`, then `task_done` — no
/// `submit_plan` anywhere in the sequence. Pre-Phase-1, the
/// `task_done` call would have been rejected by the write-gate
/// because the executor started in `Exploring`; the only way out
/// was a valid `submit_plan`. Phase 1 dropped that gate, and this
/// test pins the new contract end-to-end through the executor's
/// public surface.
#[tokio::test]
async fn write_file_then_task_done_succeeds_without_submit_plan() {
    let executor = make_executor();

    let write = ToolCallInfo {
        id: "wf_1".into(),
        name: "write_file".into(),
        input: serde_json::json!({
            "path": "src/lib.rs",
            "content": "pub fn answer() -> u32 { 42 }",
        }),
    };
    let write_results = executor.execute(&[write]).await;
    assert_eq!(write_results.len(), 1);
    assert!(
        !write_results[0].is_error,
        "write_file must succeed without submit_plan: {}",
        write_results[0].content,
    );

    // The completion guard requires a self-review read of every
    // modified file before `task_done` — re-reading is part of the
    // existing DoD-precheck contract that Phase 1 explicitly does
    // NOT change. Drive it through the same `execute` surface so
    // we don't bypass the self-review tracker.
    let read = ToolCallInfo {
        id: "rf_1".into(),
        name: "read_file".into(),
        input: serde_json::json!({"path": "src/lib.rs"}),
    };
    let read_results = executor.execute(&[read]).await;
    assert!(!read_results[0].is_error, "self-review read must succeed");

    let done = task_done_call("implemented answer()");
    let done_results = executor.execute(&[done]).await;
    assert_eq!(done_results.len(), 1);
    assert!(
        !done_results[0].is_error,
        "task_done must succeed after write_file + self-review read \
         without submit_plan: {}",
        done_results[0].content,
    );
    assert!(
        done_results[0].stop_loop,
        "successful task_done must request loop stop",
    );

    // The executor must have NEVER seen a submit_plan tool call. The
    // only way `task_done` can have succeeded is if the write-gate is
    // gone, which is the headline Phase 1 contract. The tracked
    // file-op count is checked against the executor's internal
    // mutex directly because Phase 7 dropped the cross-layer mirror
    // on `TaskExecutionResult`.
    let mut result = TaskExecutionResult::default();
    executor.merge_into_result(&mut result).await;
    assert_eq!(
        executor.tracked_file_ops.lock().await.len(),
        1,
        "executor must have tracked the write-gate-less write",
    );
    assert!(
        !*executor.no_changes_needed.lock().await,
        "no_changes_needed must stay false on the write path",
    );
}

/// `handle_submit_plan` must flip the shared exploration-reset signal
/// on the successful `Ok(())` branch so the wrapping agent loop knows
/// to zero its exploration/read-guard counters at the next iteration.
#[tokio::test]
async fn submit_plan_flips_reset_signal() {
    let signal = Arc::new(AtomicBool::new(false));
    let executor = make_exploring_executor_with_signal(Arc::clone(&signal));

    let plan_call = ToolCallInfo {
        id: "sp_1".into(),
        name: "submit_plan".into(),
        input: serde_json::json!({
            "approach": "fix the bug by adding a null check that prevents the crash",
            "files_to_modify": ["src/main.rs"],
            "key_decisions": ["use an early return"],
        }),
    };
    let results = executor.execute(&[plan_call]).await;
    assert!(!results[0].is_error, "plan should be accepted");

    assert!(
        signal.load(Ordering::Acquire),
        "reset signal must be flipped to true after successful plan acceptance"
    );
}

/// Companion: a rejected `submit_plan` must NOT flip the reset signal,
/// otherwise the agent loop would reset counters every time the model
/// submitted a malformed plan.
#[tokio::test]
async fn invalid_plan_does_not_flip_reset_signal() {
    let signal = Arc::new(AtomicBool::new(false));
    let executor = make_exploring_executor_with_signal(Arc::clone(&signal));

    let plan_call = ToolCallInfo {
        id: "sp_1".into(),
        name: "submit_plan".into(),
        input: serde_json::json!({
            "approach": "short",
            "files_to_modify": [],
        }),
    };
    let results = executor.execute(&[plan_call]).await;
    assert!(results[0].is_error, "plan should be rejected");

    assert!(
        !signal.load(Ordering::Acquire),
        "reset signal must NOT be flipped on rejected plan"
    );
}
