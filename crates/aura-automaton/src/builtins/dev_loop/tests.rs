//! Forward-event translation tests for the dev-loop.
//!
//! The simplification removed `TaskAggregate`, `validate_execution`,
//! and `commit_and_push` along with the tests that covered them. The
//! `forward_agent_event` translation layer is still load-bearing for
//! the WS event stream consumed by chat, dev-loop, and task_run, so
//! those tests stay.
//!
//! `AgentIdentityEnvelope` wire-roundtrip tests live here too: they
//! lock the JSON shape aura-os populates → harness parses → rendered
//! system prompt tags so a future schema drift on either side
//! triggers a compile / test failure rather than a silent
//! cross-repo break.

use super::{
    forward_agent_event, spawn_agent_event_forwarder, AgentIdentityEnvelope, ForwardOutcome,
};
use crate::events::AutomatonEvent;

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
fn envelope_from_json_parses_full_payload() {
    let cfg = serde_json::json!({
        "agent_identity": {
            "name": "Atlas",
            "role": "Engineer",
            "personality": "Precise and methodical.",
        },
        "agent_skills": ["Rust", "TypeScript"],
        "agent_system_prompt": "Use TDD on every change.",
    });

    let envelope = AgentIdentityEnvelope::from_json(&cfg);

    assert!(
        !envelope.is_empty(),
        "populated payload must not collapse to empty"
    );
    let info = envelope
        .as_agent_info()
        .expect("populated envelope must yield AgentInfo");
    let identity = info.identity.expect("identity present");
    assert_eq!(identity.name, "Atlas");
    assert_eq!(identity.role, "Engineer");
    assert_eq!(identity.personality, "Precise and methodical.");
    assert_eq!(info.skills, &["Rust".to_string(), "TypeScript".to_string()]);
    assert_eq!(info.system_prompt, Some("Use TDD on every change."));
}

#[test]
fn envelope_from_json_handles_missing_fields() {
    let envelope = AgentIdentityEnvelope::from_json(&serde_json::json!({}));
    assert!(
        envelope.is_empty(),
        "empty JSON object must produce an empty envelope"
    );
    assert!(
        envelope.as_agent_info().is_none(),
        "empty envelope must yield no AgentInfo so identity sections drop"
    );
}

#[test]
fn envelope_from_json_treats_blank_strings_as_empty() {
    let cfg = serde_json::json!({
        "agent_identity": {
            "name": "   ",
            "role": "",
            "personality": "\n\t",
        },
        "agent_skills": [],
        "agent_system_prompt": "   ",
    });

    let envelope = AgentIdentityEnvelope::from_json(&cfg);
    assert!(
        envelope.is_empty(),
        "blank-string fields must collapse to empty so no <agent_*> tags render"
    );
    assert!(envelope.as_agent_info().is_none());
}

#[test]
fn envelope_skills_only_still_renders_as_populated() {
    // Skills-only payloads should still render an <agent_skills> tag
    // even when identity / system prompt are absent.
    let cfg = serde_json::json!({
        "agent_skills": ["Rust"],
    });

    let envelope = AgentIdentityEnvelope::from_json(&cfg);
    assert!(!envelope.is_empty());
    let info = envelope.as_agent_info().expect("skills-only is populated");
    assert!(
        info.identity.is_none(),
        "missing identity object must leave AgentInfo.identity = None"
    );
    assert_eq!(info.skills, &["Rust".to_string()]);
    assert!(info.system_prompt.is_none());
}

#[test]
fn envelope_roundtrips_into_system_prompt_tags() {
    // End-to-end roundtrip: aura-os-shaped JSON → envelope →
    // AgentInfo → agentic_execution_system_prompt → rendered tags.
    let cfg = serde_json::json!({
        "agent_identity": {
            "name": "Atlas",
            "role": "Engineer",
            "personality": "Precise and methodical.",
        },
        "agent_skills": ["Rust", "TypeScript"],
        "agent_system_prompt": "Use TDD on every change.",
    });
    let envelope = AgentIdentityEnvelope::from_json(&cfg);
    let info = envelope.as_agent_info().expect("populated");

    let project = aura_agent::prompts::ProjectInfo {
        project_id: None,
        name: "Demo",
        description: "A demo project.",
        folder_path: "/nonexistent",
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    };
    let prompt = aura_agent::prompts::agentic_execution_system_prompt(&project, Some(&info));

    for tag in [
        "<agent_identity>",
        "</agent_identity>",
        "<agent_skills>",
        "- Rust",
        "- TypeScript",
        "</agent_skills>",
        "<agent_system_prompt>",
        "Use TDD on every change.",
        "</agent_system_prompt>",
        "<project_context>",
    ] {
        assert!(
            prompt.contains(tag),
            "expected {tag} in the rendered roundtrip prompt; got:\n{prompt}",
        );
    }
}

#[test]
fn empty_envelope_keeps_identity_sections_off() {
    let envelope = AgentIdentityEnvelope::from_json(&serde_json::json!({}));
    let info = envelope.as_agent_info();
    assert!(info.is_none());

    let project = aura_agent::prompts::ProjectInfo {
        project_id: None,
        name: "Demo",
        description: "A demo project.",
        folder_path: "/nonexistent",
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    };
    let prompt = aura_agent::prompts::agentic_execution_system_prompt(&project, info.as_ref());

    for tag in [
        "<agent_identity>",
        "<agent_skills>",
        "<agent_system_prompt>",
    ] {
        assert!(
            !prompt.contains(tag),
            "empty envelope must NOT render {tag}; got:\n{prompt}",
        );
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

// ---------------------------------------------------------------
// Post-E.4 drop-policy regression tests
// ---------------------------------------------------------------
//
// Lock in the contract documented in
// `dev_loop/forward_event.rs::ForwardOutcome` and exercised by the
// `spawn_agent_event_forwarder` consolidated drain helper:
//
// 1. `forward_agent_event` reports `Full` / `Closed` outcomes
//    without logging (`forward_agent_event_reports_full` /
//    `_reports_closed`). The pre-fix call site warned per-event;
//    after the fix the function returns the typed outcome and the
//    drain helper decides whether to log.
// 2. `spawn_agent_event_forwarder` keeps consuming the inner
//    channel after the outer receiver drops — the regression that
//    let the agent loop's own `try_send` accumulate backpressure
//    and ultimately fail the tick (`forwarder_drains_inner_after_outer_closed`).
// 3. The forwarder doesn't lose protocol events in an
//    end-to-end burst when the outer channel has slack
//    (`forwarder_forwards_all_events_when_outer_has_capacity`).

#[test]
fn forward_agent_event_reports_sent_when_outer_has_capacity() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(8);
    let outcome = forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("hi".to_string()),
        Some("task-x"),
    );
    assert_eq!(outcome, ForwardOutcome::Sent);
    rx.try_recv().expect("event must be on the outer channel");
}

#[test]
fn forward_agent_event_reports_full_without_panicking() {
    // Capacity-1 outer channel + an already-buffered event guarantees
    // the next `try_send` returns `TrySendError::Full`. Before the
    // fix this branch warned per call; the new contract returns
    // `DroppedFull` and lets the caller debounce.
    let (tx, _rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(1);
    let outcome_first = forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("first".to_string()),
        Some("task-x"),
    );
    assert_eq!(outcome_first, ForwardOutcome::Sent);
    let outcome_second = forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("second".to_string()),
        Some("task-x"),
    );
    assert_eq!(
        outcome_second,
        ForwardOutcome::DroppedFull,
        "second send must observe TrySendError::Full"
    );
}

#[test]
fn forward_agent_event_reports_closed_when_receiver_dropped() {
    // The pre-fix symptom: receiver dropped mid-task and every
    // subsequent per-delta forward warned. Lock in that the
    // function now returns the typed outcome.
    let (tx, rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(8);
    drop(rx);
    let outcome = forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::TextDelta("orphaned".to_string()),
        Some("task-x"),
    );
    assert_eq!(outcome, ForwardOutcome::DroppedClosed);
}

#[test]
fn forward_agent_event_reports_ignored_for_unprojected_variant() {
    // `forward_agent_event`'s `_ => return ForwardOutcome::Ignored`
    // arm covers `AgentLoopEvent` variants we don't currently
    // surface on the WS stream (e.g. `Started`). Without this the
    // forwarder's accounting (full / closed thresholds) would be
    // off by N per task.
    let (tx, _rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(8);
    // `ThinkingComplete` is intentionally not projected onto
    // `AutomatonEvent` (no WS-side consumer needs it), so it
    // exercises the wildcard arm.
    let outcome = forward_agent_event(
        &tx,
        aura_agent::AgentLoopEvent::ThinkingComplete,
        Some("task-x"),
    );
    assert_eq!(outcome, ForwardOutcome::Ignored);
}

#[tokio::test(flavor = "current_thread")]
async fn forwarder_drains_inner_after_outer_closed() {
    // Headline regression test.
    //
    // Reproduce the post-E.4 flood by dropping the outer receiver
    // mid-stream and then flooding the inner channel with 200
    // advisory events (the same shape the streaming pump produces:
    // per-delta `TextDelta` per `OutputItemDone`).
    //
    // Pre-fix: each event triggered `forward_agent_event`'s
    // per-call `warn!("automaton event channel full or closed: ...")`,
    // and the inner-channel drain task in `tick.rs` had no special
    // handling — the warnings simply piled up while the agent loop
    // continued to call `event_tx.try_send` for the next 6+
    // samplings. That's the 40+ WARN lines per task in the
    // operator report.
    //
    // Post-fix: `spawn_agent_event_forwarder` observes the closed
    // outer once (single `debug!`), then keeps draining the inner
    // channel so the agent loop's `try_send` never sees a full
    // inner queue. This test asserts the inner channel is fully
    // drained (`recv()` returns `None` exactly when all 200
    // senders' messages have been consumed) and the spawned task
    // exits cleanly when the inner sender is dropped.
    let (outer_tx, outer_rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(2);
    let (inner_tx, inner_rx) = tokio::sync::mpsc::channel::<aura_agent::AgentLoopEvent>(64);
    drop(outer_rx);

    let handle = spawn_agent_event_forwarder(outer_tx, inner_rx, Some("task-x".to_string()));

    // Flood the inner channel with the per-delta event shape that
    // E.4's streaming pump produces. 200 chosen to comfortably
    // exceed both the inner-channel capacity (64) and the
    // power-of-two log threshold cadence (1, 2, 4, …, 128) the
    // forwarder uses internally.
    for i in 0..200u32 {
        inner_tx
            .send(aura_agent::AgentLoopEvent::TextDelta(format!("d{i}")))
            .await
            .expect(
                "inner send must succeed: the forwarder keeps draining \
                 even after the outer receiver dropped, so the inner \
                 channel never backs up",
            );
    }
    drop(inner_tx);

    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("forwarder must exit within 5s of the inner sender dropping")
        .expect("forwarder task must not panic");
}

#[tokio::test(flavor = "current_thread")]
async fn forwarder_forwards_all_events_when_outer_has_capacity() {
    // Positive-path regression: with a sized outer channel the
    // forwarder must project every advisory event through
    // `forward_agent_event`. Lock this so a future refactor of
    // `spawn_agent_event_forwarder` (e.g. coalescing `TextDelta`s)
    // doesn't silently drop events on the happy path.
    let (outer_tx, mut outer_rx) = tokio::sync::mpsc::channel::<AutomatonEvent>(512);
    let (inner_tx, inner_rx) = tokio::sync::mpsc::channel::<aura_agent::AgentLoopEvent>(64);

    let handle = spawn_agent_event_forwarder(outer_tx, inner_rx, Some("task-x".to_string()));

    for i in 0..32u32 {
        inner_tx
            .send(aura_agent::AgentLoopEvent::TextDelta(format!("d{i}")))
            .await
            .expect("inner send must succeed");
    }
    drop(inner_tx);

    handle.await.expect("forwarder task must not panic");

    let mut received = 0usize;
    while let Ok(event) = outer_rx.try_recv() {
        match event {
            AutomatonEvent::TextDelta { task_id, .. } => {
                assert_eq!(task_id.as_deref(), Some("task-x"));
                received += 1;
            }
            other => panic!("unexpected projection: {other:?}"),
        }
    }
    assert_eq!(
        received, 32,
        "all 32 per-delta events must reach the outer channel when it has capacity"
    );
}
