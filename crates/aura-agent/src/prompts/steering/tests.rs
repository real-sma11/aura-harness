//! Per-kind envelope tests for [`SteeringInjector`].
//!
//! Each test asserts that the rendered body for a given
//! [`SteeringKind`] is wrapped in the canonical
//! `<harness_steering kind="…">…</harness_steering>` envelope, and that
//! the inner wording is preserved verbatim from the pre-PR-D inline
//! call sites in `task_executor`. These tests act as the wording lock
//! for PR D — if a future change rewords a steering body the test
//! needs to be updated in lockstep.

use super::{
    EarlyTestOracle, RepeatedReadTracker, SteeringInjector, SteeringKind, REPEATED_READ_THRESHOLD,
};
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

// ---------------------------------------------------------------------------
// Phase 3b: RepeatedReadTracker tests
// ---------------------------------------------------------------------------

#[test]
fn render_repeated_read_wraps_body_and_surfaces_short_hash() {
    let rendered = SteeringInjector::render(&SteeringKind::RepeatedRead {
        content_hash: "deadbeefcafef00d".into(),
    });
    assert_envelope(&rendered, "repeated_read");
    assert!(
        rendered.contains("content_hash=deadbeef"),
        "repeated-read body should surface the leading 8 hex chars of the hash:\n{rendered}"
    );
    assert!(
        rendered.contains("3 times this turn"),
        "repeated-read body should name the firing threshold:\n{rendered}"
    );
    assert!(
        rendered.contains("`start_line`/`end_line`"),
        "repeated-read body should suggest the narrow-range alternative:\n{rendered}"
    );
}

#[test]
fn steering_fires_after_three_identical_content_hash_reads() {
    let mut tracker = RepeatedReadTracker::new();

    // First two reads must NOT enqueue anything: the threshold is 3.
    assert!(!tracker.record("hash_a"));
    assert_eq!(tracker.pending_count(), 0);
    assert!(!tracker.record("hash_a"));
    assert_eq!(tracker.pending_count(), 0);

    // Third read crosses the threshold and queues exactly one nudge.
    assert!(
        tracker.record("hash_a"),
        "third identical read must report 'queued' so the agent loop knows a nudge is pending"
    );
    assert_eq!(tracker.pending_count(), 1);

    // Fourth read in the SAME turn must NOT enqueue an extra nudge —
    // the (turn, content_hash) pair has already fired.
    assert!(
        !tracker.record("hash_a"),
        "4th identical read in the same turn must not re-fire the nudge"
    );
    assert_eq!(
        tracker.pending_count(),
        1,
        "fourth read must not enqueue an additional nudge for the same hash"
    );

    // Begin the next turn: drain the queued nudges and reset counts.
    let nudges = tracker.begin_turn();
    assert_eq!(
        nudges.len(),
        1,
        "exactly one nudge should be drained for the (turn, content_hash) pair that crossed the threshold"
    );
    match &nudges[0] {
        SteeringKind::RepeatedRead { content_hash } => {
            assert_eq!(content_hash, "hash_a");
        }
        other => panic!("unexpected steering kind drained from tracker: {other:?}"),
    }

    // After draining, a fresh turn with no recorded reads yields nothing.
    let next = tracker.begin_turn();
    assert!(
        next.is_empty(),
        "turn boundary with no new reads must produce no nudges"
    );
}

#[test]
fn repeated_read_tracker_resets_per_turn_counts() {
    let mut tracker = RepeatedReadTracker::new();
    // Two reads in turn 1 — below threshold.
    tracker.record("hash_a");
    tracker.record("hash_a");
    assert_eq!(tracker.pending_count(), 0);

    // Turn boundary clears the count for hash_a.
    let drained = tracker.begin_turn();
    assert!(drained.is_empty());

    // Two more reads in turn 2 — still below threshold because the
    // counter reset.
    tracker.record("hash_a");
    tracker.record("hash_a");
    assert_eq!(
        tracker.pending_count(),
        0,
        "per-turn counts must reset on begin_turn so repeats only fire when 3 land in one turn"
    );
}

#[test]
fn repeated_read_tracker_isolates_distinct_hashes() {
    let mut tracker = RepeatedReadTracker::new();
    for _ in 0..REPEATED_READ_THRESHOLD {
        tracker.record("hash_a");
    }
    for _ in 0..(REPEATED_READ_THRESHOLD - 1) {
        tracker.record("hash_b");
    }
    let nudges = tracker.begin_turn();
    assert_eq!(
        nudges.len(),
        1,
        "hash_b stayed below threshold, only hash_a should fire"
    );
    match &nudges[0] {
        SteeringKind::RepeatedRead { content_hash } => assert_eq!(content_hash, "hash_a"),
        other => panic!("unexpected steering kind drained from tracker: {other:?}"),
    }
}

#[test]
fn repeated_read_tracker_ignores_empty_hash() {
    let mut tracker = RepeatedReadTracker::new();
    for _ in 0..(REPEATED_READ_THRESHOLD * 2) {
        assert!(!tracker.record(""));
    }
    assert_eq!(
        tracker.pending_count(),
        0,
        "empty content_hash must not enqueue nudges (defensive against tools that omit the metadata)"
    );
}

// ---------------------------------------------------------------------------
// Phase 3a: EarlyTestOracle tests (minimum-viable, hint-only)
// ---------------------------------------------------------------------------

#[test]
fn render_task_already_satisfied_hint_wraps_body_and_carries_command() {
    let rendered = SteeringInjector::render(&SteeringKind::TaskAlreadySatisfiedHint {
        test_command: "cargo --version".into(),
    });
    assert_envelope(&rendered, "task_already_satisfied");
    assert!(
        rendered.contains("test_command: \"cargo --version\""),
        "rendered body must carry the test_command verbatim:\n{rendered}"
    );
    assert!(
        rendered.contains("test-augmentation mode"),
        "hint body must steer the model toward test-augmentation when the gate already passes:\n{rendered}"
    );
    assert!(
        rendered.contains("harness has not run this command"),
        "minimum-viable variant must explicitly say the test was NOT executed by the harness:\n{rendered}"
    );
}

#[test]
fn early_oracle_emits_hint_after_first_read_only_batch_when_test_command_declared() {
    let mut oracle = EarlyTestOracle::new(Some("cargo --version".into()), true);

    assert!(
        oracle.is_armed(),
        "oracle must be armed when enabled and a test_command is declared"
    );
    assert!(
        oracle.take_hint().is_none(),
        "no hint should be emitted before any read-only batch is observed"
    );

    // First read-only batch: a couple of explorations.
    oracle.observe_tool("read_file");
    oracle.observe_tool("list_files");
    assert!(
        oracle.take_hint().is_none(),
        "no hint should be emitted while the first read-only batch is still open"
    );

    // Boundary: the first non-read tool closes the batch.
    oracle.observe_tool("edit_file");

    let hint = oracle
        .take_hint()
        .expect("hint must be emitted when the first read-only batch closes");
    match hint {
        SteeringKind::TaskAlreadySatisfiedHint { test_command } => {
            assert_eq!(
                test_command, "cargo --version",
                "test_command must round-trip into the hint payload verbatim"
            );
        }
        other => panic!("unexpected steering kind from oracle: {other:?}"),
    }

    assert!(
        !oracle.is_armed(),
        "oracle must disarm itself after firing exactly once"
    );
    assert!(
        oracle.take_hint().is_none(),
        "second take_hint call must return None — the oracle is single-shot"
    );
}

#[test]
fn early_oracle_close_batch_explicit_boundary_fires_hint() {
    let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), true);
    oracle.observe_tool("read_file");
    oracle.observe_tool("read_file");
    oracle.close_batch();
    let hint = oracle
        .take_hint()
        .expect("explicit close_batch must queue the hint identically to a write boundary");
    matches!(hint, SteeringKind::TaskAlreadySatisfiedHint { .. });
}

#[test]
fn early_oracle_disabled_never_fires() {
    let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), false);
    assert!(!oracle.is_armed());
    oracle.observe_tool("read_file");
    oracle.observe_tool("write_file");
    assert!(oracle.take_hint().is_none());
}

#[test]
fn early_oracle_without_test_command_never_fires() {
    let mut oracle = EarlyTestOracle::new(None, true);
    assert!(!oracle.is_armed());
    oracle.observe_tool("read_file");
    oracle.observe_tool("write_file");
    assert!(oracle.take_hint().is_none());
}

#[test]
fn early_oracle_with_blank_test_command_never_fires() {
    let oracle = EarlyTestOracle::new(Some("   ".into()), true);
    assert!(
        !oracle.is_armed(),
        "blank test_command must short-circuit to the disarmed state"
    );
}

#[test]
fn early_oracle_write_first_disarms_without_firing() {
    // The hint targets the read-heavy pre-edit phase — if the agent
    // dives straight into a write with no exploration, there's
    // nothing to nudge about.
    let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), true);
    oracle.observe_tool("write_file");
    assert!(oracle.take_hint().is_none());
    assert!(!oracle.is_armed());
}
