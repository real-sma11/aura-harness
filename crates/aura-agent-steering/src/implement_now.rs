//! One-shot steering when dev-loop tasks accumulate exploration
//! tools without writes.
//!
//! Phase 2 relocated this evaluator from `prompts/steering/` (where
//! it imported `LoopState` / `AgentLoopConfig` from the agent loop —
//! a layer violation) into the agent loop itself. Phase 5 added
//! [`ImplementNowSteering`]: a [`super::TurnSteering`] wrapper that
//! owns its own exploration / write / read-path telemetry so the
//! trait-driven path can drop the
//! `evaluate_implement_now(config, state)` borrow on `LoopState`.
//! Phase 6a moved it again — out of `aura-agent` and into the
//! dedicated `aura-agent-steering` crate — so the steering layer
//! sits strictly below the agent loop.

use std::collections::HashSet;
use std::path::PathBuf;

use aura_context_prompts::SteeringKind;

use crate::helpers::{is_exploration_tool, is_write_tool};
use crate::registry::TurnSteering;
use crate::types::{ToolCallInfo, ToolCallResult};

fn sample_read_paths(session_read_paths: &HashSet<PathBuf>) -> Vec<String> {
    let mut paths: Vec<String> = session_read_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    paths.sort();
    paths.truncate(aura_config::IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE);
    paths
}

/// Self-contained [`TurnSteering`] source that arms a one-shot
/// [`SteeringKind::ImplementNow`] when cumulative exploration calls
/// cross [`aura_config::SteeringConfig::implement_now_threshold`]
/// without any file write landing.
///
/// The source observes every `(tool, result)` pair through
/// [`TurnSteering::observe_tool`] and maintains its own
/// `(exploration_count, had_any_file_write, session_read_paths)`
/// counters rather than borrowing them from the agent loop's
/// `LoopState` — keeping every per-source state self-contained per
/// the Phase 5 design goal. The agent loop's `LoopState` still
/// maintains parallel `exploration_state` / `had_any_file_write`
/// fields for other consumers
/// (`tool_pipeline::partition_circling_duplicate_reads`,
/// `compute_thinking_effort`); the two stay in lockstep because
/// both are driven by `tool_pipeline::track_tool_effects` in the
/// same call.
#[derive(Debug)]
pub struct ImplementNowSteering {
    /// Whether the wrapping agent loop declared a phase-reset signal.
    /// Tracked tasks (the dev-loop / TaskRun path) set this `true`;
    /// chat and generic callers leave it `false`, in which case the
    /// gate never fires.
    has_phase_reset_signal: bool,
    /// One-shot latch: set once the gate has queued its nudge.
    fired: bool,
    /// Successful exploration-tool calls observed so far. Mirrors
    /// `LoopState::exploration_state.count` but lives here so the
    /// source is self-contained.
    exploration_count: usize,
    /// True once any successful write tool has been observed.
    /// Mirrors `LoopState::had_any_file_write`.
    had_any_file_write: bool,
    /// Distinct paths the agent has successfully read this run, used
    /// to populate the `sample_paths` field of the rendered nudge.
    session_read_paths: HashSet<PathBuf>,
    /// Pending nudge queued in [`Self::begin_turn`] for drain in the
    /// next [`Self::drain_for_next_turn`] call.
    pending: Vec<SteeringKind>,
}

impl ImplementNowSteering {
    /// Construct a new source. `has_phase_reset_signal` mirrors the
    /// agent loop's `AgentLoopConfig::phase_reset_signal.is_some()`
    /// — the dev-loop task path threads a shared
    /// `Arc<AtomicBool>` and sets this `true`; chat / generic
    /// callers leave it `false` and the gate stays permanently
    /// disarmed.
    #[must_use]
    pub fn new(has_phase_reset_signal: bool) -> Self {
        Self {
            has_phase_reset_signal,
            fired: false,
            exploration_count: 0,
            had_any_file_write: false,
            session_read_paths: HashSet::new(),
            pending: Vec::new(),
        }
    }
}

impl TurnSteering for ImplementNowSteering {
    fn observe_tool(&mut self, tool: &ToolCallInfo, result: &ToolCallResult) {
        if is_exploration_tool(&tool.name) {
            self.exploration_count += 1;
            if !result.is_error {
                if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                    self.session_read_paths.insert(PathBuf::from(path));
                }
            }
        }

        if is_write_tool(&tool.name) && !result.is_error {
            let has_path = tool.input.get("path").and_then(|v| v.as_str()).is_some();
            if has_path || !result.file_changes.is_empty() {
                self.had_any_file_write = true;
                if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                    self.session_read_paths.remove(&PathBuf::from(path));
                }
                for change in &result.file_changes {
                    self.session_read_paths.remove(&PathBuf::from(&change.path));
                }
            }
        }
    }

    fn begin_turn(&mut self) {
        if self.fired {
            return;
        }
        let steering = &aura_config::agent().steering;
        if !steering.implement_now_enabled {
            return;
        }
        if !self.has_phase_reset_signal {
            return;
        }
        if self.had_any_file_write {
            return;
        }
        if self.exploration_count < steering.implement_now_threshold {
            return;
        }
        self.pending.push(SteeringKind::ImplementNow {
            exploration_count: self.exploration_count,
            sample_paths: sample_read_paths(&self.session_read_paths),
        });
        self.fired = true;
    }

    fn drain_for_next_turn(&mut self) -> Vec<SteeringKind> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileChange, FileChangeKind};
    use serde_json::json;

    fn read_tool(path: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: format!("toolu_read_{path}"),
            name: "read_file".to_string(),
            input: json!({"path": path}),
        }
    }

    fn ok_result(id: &str) -> ToolCallResult {
        ToolCallResult {
            tool_use_id: id.to_string(),
            content: "ok".to_string(),
            is_error: false,
            kind: aura_core_types::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        }
    }

    #[test]
    fn impl_now_self_contained_fires_at_threshold() {
        let mut src = ImplementNowSteering::new(true);
        let threshold = aura_config::agent().steering.implement_now_threshold;
        for i in 0..threshold {
            let path = format!("src/file_{i}.rs");
            let tool = read_tool(&path);
            let res = ok_result(&tool.id);
            <ImplementNowSteering as TurnSteering>::observe_tool(&mut src, &tool, &res);
        }
        <ImplementNowSteering as TurnSteering>::begin_turn(&mut src);
        let kinds = <ImplementNowSteering as TurnSteering>::drain_for_next_turn(&mut src);
        assert_eq!(kinds.len(), 1, "gate should fire once at threshold");
        match &kinds[0] {
            SteeringKind::ImplementNow {
                exploration_count, ..
            } => assert_eq!(*exploration_count, threshold),
            other => panic!("unexpected kind: {other:?}"),
        }
        <ImplementNowSteering as TurnSteering>::begin_turn(&mut src);
        let second = <ImplementNowSteering as TurnSteering>::drain_for_next_turn(&mut src);
        assert!(
            second.is_empty(),
            "implement_now is one-shot per run, must not re-fire"
        );
    }

    #[test]
    fn impl_now_self_contained_skipped_without_signal() {
        let mut src = ImplementNowSteering::new(false);
        let threshold = aura_config::agent().steering.implement_now_threshold;
        for i in 0..(threshold + 5) {
            let path = format!("src/file_{i}.rs");
            let tool = read_tool(&path);
            let res = ok_result(&tool.id);
            <ImplementNowSteering as TurnSteering>::observe_tool(&mut src, &tool, &res);
        }
        <ImplementNowSteering as TurnSteering>::begin_turn(&mut src);
        assert!(
            <ImplementNowSteering as TurnSteering>::drain_for_next_turn(&mut src).is_empty(),
            "gate must stay disarmed for chat / generic callers"
        );
    }

    #[test]
    fn impl_now_self_contained_skipped_after_write() {
        let mut src = ImplementNowSteering::new(true);
        let threshold = aura_config::agent().steering.implement_now_threshold;
        for i in 0..threshold {
            let path = format!("src/file_{i}.rs");
            let tool = read_tool(&path);
            let res = ok_result(&tool.id);
            <ImplementNowSteering as TurnSteering>::observe_tool(&mut src, &tool, &res);
        }
        let write_tool = ToolCallInfo {
            id: "toolu_write".into(),
            name: "edit_file".into(),
            input: json!({"path": "src/file_0.rs"}),
        };
        let mut write_res = ok_result(&write_tool.id);
        write_res.file_changes = vec![FileChange {
            path: "src/file_0.rs".into(),
            kind: FileChangeKind::Modify,
            lines_added: 1,
            lines_removed: 0,
        }];
        <ImplementNowSteering as TurnSteering>::observe_tool(&mut src, &write_tool, &write_res);
        <ImplementNowSteering as TurnSteering>::begin_turn(&mut src);
        assert!(
            <ImplementNowSteering as TurnSteering>::drain_for_next_turn(&mut src).is_empty(),
            "a successful write must disarm the implement-now gate"
        );
    }

    #[test]
    fn impl_now_self_contained_includes_sample_paths_sorted() {
        let mut src = ImplementNowSteering::new(true);
        let threshold = aura_config::agent().steering.implement_now_threshold;
        let storage = read_tool("crates/zero-storage/src/storage.rs");
        let outbox = read_tool("crates/zero-storage/src/outbox.rs");
        <ImplementNowSteering as TurnSteering>::observe_tool(
            &mut src,
            &storage,
            &ok_result(&storage.id),
        );
        <ImplementNowSteering as TurnSteering>::observe_tool(
            &mut src,
            &outbox,
            &ok_result(&outbox.id),
        );
        for i in 2..threshold {
            let path = format!("src/throwaway_{i}.rs");
            let tool = read_tool(&path);
            <ImplementNowSteering as TurnSteering>::observe_tool(
                &mut src,
                &tool,
                &ok_result(&tool.id),
            );
        }
        <ImplementNowSteering as TurnSteering>::begin_turn(&mut src);
        let kinds = <ImplementNowSteering as TurnSteering>::drain_for_next_turn(&mut src);
        assert_eq!(kinds.len(), 1);
        match &kinds[0] {
            SteeringKind::ImplementNow { sample_paths, .. } => {
                assert!(
                    sample_paths.contains(&"crates/zero-storage/src/outbox.rs".to_string()),
                    "outbox path should be in sample paths"
                );
                assert!(
                    sample_paths.contains(&"crates/zero-storage/src/storage.rs".to_string()),
                    "storage path should be in sample paths"
                );
                assert!(
                    sample_paths.len() <= aura_config::IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE,
                    "sample paths must be capped"
                );
                let mut sorted = sample_paths.clone();
                sorted.sort();
                assert_eq!(sample_paths, &sorted, "sample paths must be sorted");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }
}
