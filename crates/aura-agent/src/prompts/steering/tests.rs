//! Per-kind envelope tests for [`SteeringInjector`].
//!
//! Each test asserts that the rendered body for a given
//! [`SteeringKind`] is wrapped in the canonical
//! `<harness_steering kind="…">…</harness_steering>` envelope, and that
//! the inner wording is preserved verbatim from the pre-PR-D inline
//! call sites in `task_executor`. These tests act as the wording lock
//! for PR D — if a future change rewords a steering body the test
//! needs to be updated in lockstep.

use super::{SteeringInjector, SteeringKind};
use crate::file_ops::{StubPattern, StubReport};
use aura_reasoner::{Message, Role};

fn assert_envelope(rendered: &str, label: &str) {
    let expected_open = format!("<harness_steering kind=\"{label}\">\n");
    let expected_close = "\n</harness_steering>";
    assert!(
        rendered.starts_with(&expected_open),
        "expected envelope to open with {expected_open:?}, got:\n{rendered}"
    );
    assert!(
        rendered.ends_with(expected_close),
        "expected envelope to close with {expected_close:?}, got:\n{rendered}"
    );
}

#[test]
fn render_task_done_no_writes_wraps_body_and_preserves_wording() {
    let rendered = SteeringInjector::render(&SteeringKind::TaskDoneNoWrites);
    assert_envelope(&rendered, "task_done_rejected");
    assert!(
        rendered.contains("ERROR: task_done was rejected — you have not produced any file changes"),
        "no-writes wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("\"no_changes_needed\": true"),
        "escape-hatch wording drifted:\n{rendered}"
    );
}

#[test]
fn render_task_done_test_gate_failed_wraps_body_and_carries_attempt() {
    let rendered = SteeringInjector::render(&SteeringKind::TaskDoneTestGateFailed {
        cmd: "cargo test".into(),
        attempt: 2,
        max_attempts: 8,
        summary: "2 failed".into(),
        failures_block: "\n\nFailing tests:\n- foo::bar\n".into(),
        stderr_block: String::new(),
    });
    assert_envelope(&rendered, "task_done_rejected");
    assert!(
        rendered.contains("Definition-of-Done test gate"),
        "DoD wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("gate attempt 2/8"),
        "attempt/max interpolation drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("Failing tests:\n- foo::bar"),
        "failures block drifted:\n{rendered}"
    );
}

#[test]
fn render_task_done_test_gate_exhausted_appends_exhausted_footer() {
    let rendered = SteeringInjector::render(&SteeringKind::TaskDoneTestGateExhausted {
        cmd: "cargo test".into(),
        attempt: 8,
        max_attempts: 8,
        summary: "fail".into(),
        failures_block: String::new(),
        stderr_block: String::new(),
    });
    assert_envelope(&rendered, "task_done_rejected");
    assert!(
        rendered.contains("retry budget is exhausted"),
        "exhausted wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("dod_test_gate_exhausted=true"),
        "exhausted flag wording drifted:\n{rendered}"
    );
}

#[test]
fn render_task_done_test_gate_io_failure_includes_command_and_error() {
    let rendered = SteeringInjector::render(&SteeringKind::TaskDoneTestGateIoFailure {
        cmd: "cargo test --workspace".into(),
        error: "no such file or directory".into(),
        attempt: 1,
        max_attempts: 8,
    });
    assert_envelope(&rendered, "task_done_rejected");
    assert!(
        rendered.contains("failed to execute `cargo test --workspace`"),
        "command interpolation drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("no such file or directory"),
        "error interpolation drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("gate attempt 1/8"),
        "attempt/max interpolation drifted:\n{rendered}"
    );
}

#[test]
fn render_stub_detected_uses_existing_build_stub_fix_prompt_wording() {
    let reports = vec![StubReport {
        path: "src/lib.rs".into(),
        line: 42,
        pattern: StubPattern::TodoMacro,
        context: "fn foo() { todo!() }".into(),
    }];
    let rendered = SteeringInjector::render(&SteeringKind::StubDetected { reports });
    assert_envelope(&rendered, "stub_detected");
    assert!(
        rendered.contains("STOP: Your implementation compiles but contains stub"),
        "stub-fix preamble drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("src/lib.rs:42"),
        "stub report formatting drifted:\n{rendered}"
    );
}

#[test]
fn inject_appends_envelope_to_user_message_via_append_warning() {
    let mut messages = vec![Message::user("hello")];
    let returned = SteeringInjector::inject(&mut messages, SteeringKind::TaskDoneNoWrites);

    assert_envelope(&returned, "task_done_rejected");
    assert_eq!(
        messages.len(),
        1,
        "append_warning should fold into existing user message, not push a new one"
    );
    assert_eq!(messages[0].role, Role::User);
    let combined = messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            aura_reasoner::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        combined.contains("hello"),
        "original user content should be preserved:\n{combined}"
    );
    assert!(
        combined.contains("<harness_steering kind=\"task_done_rejected\">"),
        "envelope should be appended to the user message:\n{combined}"
    );
}

#[test]
fn inject_after_assistant_message_pushes_new_user_message() {
    let mut messages = vec![Message::assistant("hi")];
    let _returned = SteeringInjector::inject(&mut messages, SteeringKind::TaskDoneNoWrites);

    assert_eq!(
        messages.len(),
        2,
        "append_warning should push a new user message after an assistant turn"
    );
    assert_eq!(messages[1].role, Role::User);
}
