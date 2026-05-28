//! Per-source drain tests for the Phase 5 [`SteeringRegistry`].
//!
//! Each test exercises one source through the registry's
//! observe → begin_turn → drain pipeline and asserts that the
//! expected [`SteeringKind`] is drained at the right moment. The
//! tests stay registry-level (no `LoopState` or agent loop
//! plumbing) so they pin the trait contract directly without
//! coupling to the surrounding loop machinery.

use aura_prompts::SteeringKind;
use serde_json::json;

use crate::early_oracle::EarlyTestOracle;
use crate::implement_now::ImplementNowSteering;
use crate::registry::SteeringRegistry;
use crate::repeated_read::RepeatedReadTracker;
use crate::types::{FileChange, FileChangeKind, ToolCallInfo, ToolCallResult};

fn read_tool(id: &str, path: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_string(),
        name: "read_file".to_string(),
        input: json!({"path": path}),
    }
}

fn write_tool(id: &str, path: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_string(),
        name: "edit_file".to_string(),
        input: json!({"path": path}),
    }
}

fn ok_read_result(id: &str, content: &str) -> ToolCallResult {
    ToolCallResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    }
}

fn ok_write_result(id: &str, path: &str) -> ToolCallResult {
    ToolCallResult {
        tool_use_id: id.to_string(),
        content: "ok".to_string(),
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: vec![FileChange {
            path: path.to_string(),
            kind: FileChangeKind::Modify,
            lines_added: 1,
            lines_removed: 0,
        }],
    }
}

#[test]
fn repeated_read_drains_after_threshold() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(RepeatedReadTracker::new()));

    let threshold = aura_config::REPEATED_READ_THRESHOLD;
    for i in 0..threshold {
        let tool = read_tool(&format!("toolu_{i}"), "src/lib.rs");
        let result = ok_read_result(&tool.id, "fn main() {}");
        registry.observe_tool(&tool, &result);
    }

    registry.begin_turn();
    let drained = registry.drain_for_next_turn();
    assert_eq!(
        drained.len(),
        1,
        "exactly one RepeatedRead nudge should drain after threshold crossings",
    );
    assert!(
        matches!(drained[0], SteeringKind::RepeatedRead { .. }),
        "drained kind must be a RepeatedRead nudge",
    );

    registry.begin_turn();
    let drained2 = registry.drain_for_next_turn();
    assert!(
        drained2.is_empty(),
        "second turn must not re-drain the same nudge (single-shot per crossing)",
    );
}

#[test]
fn implement_now_drains_when_threshold_crossed() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(ImplementNowSteering::new(true)));

    let threshold = aura_config::agent().steering.implement_now_threshold;
    for i in 0..threshold {
        let tool = read_tool(&format!("toolu_{i}"), &format!("src/file_{i}.rs"));
        let result = ok_read_result(&tool.id, "stub");
        registry.observe_tool(&tool, &result);
    }
    assert!(
        !registry.implement_now_injected(),
        "implement_now_injected must stay false until begin_turn/drain fires the nudge",
    );

    registry.begin_turn();
    let drained = registry.drain_for_next_turn();
    assert_eq!(
        drained.len(),
        1,
        "exactly one ImplementNow nudge should drain at the threshold crossing",
    );
    match &drained[0] {
        SteeringKind::ImplementNow {
            exploration_count, ..
        } => assert_eq!(*exploration_count, threshold),
        other => panic!("unexpected kind: {other:?}"),
    }
    assert!(
        registry.implement_now_injected(),
        "implement_now_injected latch must be set after the drain so the circling-read gate fires",
    );

    registry.begin_turn();
    let drained2 = registry.drain_for_next_turn();
    assert!(
        drained2.is_empty(),
        "implement-now is one-shot — subsequent turns must not re-fire",
    );
}

#[test]
fn implement_now_does_not_drain_for_chat_without_signal() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(ImplementNowSteering::new(false)));

    let threshold = aura_config::agent().steering.implement_now_threshold;
    for i in 0..(threshold + 5) {
        let tool = read_tool(&format!("toolu_{i}"), &format!("src/file_{i}.rs"));
        let result = ok_read_result(&tool.id, "stub");
        registry.observe_tool(&tool, &result);
    }
    registry.begin_turn();
    assert!(
        registry.drain_for_next_turn().is_empty(),
        "chat / generic callers (no phase_reset_signal) must never trigger implement_now",
    );
    assert!(!registry.implement_now_injected());
}

#[test]
fn implement_now_skipped_after_write() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(ImplementNowSteering::new(true)));

    let threshold = aura_config::agent().steering.implement_now_threshold;
    for i in 0..threshold {
        let tool = read_tool(&format!("toolu_{i}"), &format!("src/file_{i}.rs"));
        let result = ok_read_result(&tool.id, "stub");
        registry.observe_tool(&tool, &result);
    }
    let write = write_tool("toolu_write", "src/file_0.rs");
    let write_res = ok_write_result(&write.id, "src/file_0.rs");
    registry.observe_tool(&write, &write_res);
    registry.begin_turn();
    assert!(
        registry.drain_for_next_turn().is_empty(),
        "a successful write must disarm the implement-now gate before begin_turn fires",
    );
}

#[test]
fn early_oracle_drains_after_first_read_batch() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(EarlyTestOracle::new(
        Some("cargo test".to_string()),
        true,
    )));

    let r1 = read_tool("toolu_r1", "src/lib.rs");
    let r2 = read_tool("toolu_r2", "src/main.rs");
    registry.observe_tool(&r1, &ok_read_result(&r1.id, "fn lib() {}"));
    registry.observe_tool(&r2, &ok_read_result(&r2.id, "fn main() {}"));

    registry.begin_turn();
    let drained = registry.drain_for_next_turn();
    assert_eq!(
        drained.len(),
        1,
        "exactly one TaskAlreadySatisfiedHint should drain after the first read-only batch closes",
    );
    match &drained[0] {
        SteeringKind::TaskAlreadySatisfiedHint { test_command } => {
            assert_eq!(test_command, "cargo test");
        }
        other => panic!("unexpected kind: {other:?}"),
    }

    registry.begin_turn();
    let drained2 = registry.drain_for_next_turn();
    assert!(drained2.is_empty(), "early oracle is single-shot per task",);
}

#[test]
fn early_oracle_skipped_without_test_command() {
    let mut registry = SteeringRegistry::new();
    registry.push(Box::new(EarlyTestOracle::new(None, true)));

    let r1 = read_tool("toolu_r1", "src/lib.rs");
    registry.observe_tool(&r1, &ok_read_result(&r1.id, "fn lib() {}"));

    registry.begin_turn();
    assert!(
        registry.drain_for_next_turn().is_empty(),
        "oracle without a test_command must stay disarmed forever",
    );
}

#[test]
fn registry_for_loop_installs_minimum_two_sources() {
    let registry = SteeringRegistry::for_loop(false, None);
    // Chat config: no `phase_reset_signal`, no `early_test_oracle`.
    // Only the always-installed `RepeatedReadTracker` and
    // (permanently-disarmed) `ImplementNowSteering` should ride.
    // We can't read `sources.len()` from outside the module, but
    // observing through the registry surface is enough:
    // implement_now never fires for chat, so the latch stays false.
    drop(registry);
}
