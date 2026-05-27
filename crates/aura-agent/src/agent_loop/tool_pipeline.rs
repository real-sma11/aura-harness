//! Inner tool-execution pipeline.
//!
//! `super::tool_execution::handle_tool_use` is the outer dispatch step
//! for `StopReason::ToolUse` (cache lookup, event emission, termination
//! checks). When that step has tool calls left to actually run, it
//! delegates here, to [`AgentLoop::process_tool_results`], which is the
//! pipeline that:
//!
//! 1. Pre-dispatch chunk guard for oversized `write_file` calls.
//! 2. Hands the tool calls to the [`AgentToolExecutor`].
//! 3. Tracks write outcomes (`any_write_success` latch + file-change journal).
//! 4. Optional auto-build after a successful write.
//!
//! Renamed from `tool_processing` in Phase 4: the old name was
//! confusingly close to `tool_execution`, even though this module sits
//! inside that one. "Pipeline" makes the multi-stage flow explicit and
//! preserves the outer/inner split between the two files.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::budget::ExplorationState;
use crate::build;
use crate::constants::WRITE_FILE_CHUNK_BYTES;
use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::types::{
    AgentLoopResult, AgentToolExecutor, BuildBaseline, ToolCallInfo, ToolCallResult,
};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tracing::warn;

use super::streaming::emit as emit_event;
use super::{AgentLoop, AgentLoopConfig, LoopState};

/// Env var that overrides the tool-running heartbeat cadence. Mirrors
/// the same name aura-os Phase 3 already published in
/// `apps/aura-os-server/src/handlers/agents/chat/turn_slot.rs` so the
/// two sides stay aligned without per-process drift.
const TOOL_HEARTBEAT_INTERVAL_ENV: &str = "AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS";

/// Default cadence (10s) when the env var is unset or unparseable.
/// Matches `DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS` on the aura-os side
/// so the harness emits a heartbeat well inside the server's
/// sliding-idle window (`AURA_TURN_MAX_TIMEOUT_SECS`, default 180s).
const DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Minimum cadence: zero would degenerate into a hot loop and
/// sub-second cadences would drown the broadcast in heartbeats.
const MIN_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 1;

/// Maximum cadence: ten minutes already exceeds the documented server
/// idle ceiling, so values past this would defeat the heartbeat's
/// purpose. Clamping protects against typos that would silently
/// disable forward-progress signalling.
const MAX_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 600;
const READS_AFTER_WRITE_ALLOWANCE: u8 = 3;

pub(super) fn read_tool_heartbeat_interval_from_env() -> Duration {
    let secs = match std::env::var(TOOL_HEARTBEAT_INTERVAL_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(parsed) => parsed.clamp(
                MIN_TOOL_HEARTBEAT_INTERVAL_SECS,
                MAX_TOOL_HEARTBEAT_INTERVAL_SECS,
            ),
            Err(_) => DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS,
        },
        Err(_) => DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS,
    };
    Duration::from_secs(secs)
}

/// Cached resolved cadence so the env lookup happens once per process.
/// Matches the `OnceLock` pattern used elsewhere in the codebase
/// (`tool_heartbeat_interval` on the aura-os side, `max_pending_turns`,
/// `read_broadcast_capacity_from_env`) so tooling that scrapes
/// configuration knobs sees a consistent shape.
pub(crate) fn tool_heartbeat_interval() -> Duration {
    static CACHED: OnceLock<Duration> = OnceLock::new();
    *CACHED.get_or_init(read_tool_heartbeat_interval_from_env)
}

/// Spawn a background task that emits an
/// [`AgentLoopEvent::Progress`] heartbeat every
/// [`tool_heartbeat_interval`] while a batch of tool calls is in
/// flight. Returns a [`HeartbeatGuard`] whose `Drop` aborts the task
/// — callers get cancel-on-completion semantics for free without
/// having to wire `tokio::select!` around every executor call.
///
/// The first heartbeat fires after the first interval tick (i.e. the
/// "cool" 0..interval window stays silent), so a tool that completes
/// inside the interval never emits one. After that the cadence is
/// strictly periodic; each tick reports `tool_running`,
/// `tool_name=<first-tool-of-batch>`, `elapsed_ms` since batch start.
///
/// `event_tx` is `None` for the headless code path (no event channel,
/// e.g. unit tests in non-streaming mode); the spawn is skipped and
/// the returned guard is a no-op so the call site stays branch-free.
pub(super) fn spawn_tool_heartbeat(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    to_execute: &[ToolCallInfo],
    interval: Duration,
) -> HeartbeatGuard {
    let Some(tx) = event_tx.cloned() else {
        return HeartbeatGuard { handle: None };
    };
    if to_execute.is_empty() {
        return HeartbeatGuard { handle: None };
    }
    // Most batches contain a single tool call; when the model emits
    // multiple in one turn we report the first one's name on the
    // heartbeat. The server-side watchdog only cares about *some*
    // forward-progress event arriving, and the chat client renders
    // the stage label, so a single representative name is enough to
    // keep the UI honest without generating one heartbeat per tool.
    let tool_name = to_execute[0].name.clone();
    let started_at = Instant::now();
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; consume it so the heartbeat
        // only emits after the configured wall-clock window has
        // elapsed (matching the documented "long tool calls > 10s"
        // contract).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let event = AgentLoopEvent::Progress {
                stage: "tool_running".to_string(),
                tool_name: Some(tool_name.clone()),
                elapsed_ms: Some(elapsed_ms),
                message: None,
            };
            // `send` (with backpressure) over `try_send` so a
            // momentarily-full broadcast doesn't drop a heartbeat —
            // dropping one defeats the watchdog-friendliness contract.
            if tx.send(event).await.is_err() {
                // Receiver gone (turn already finalized): stop ticking.
                break;
            }
        }
    });
    HeartbeatGuard {
        handle: Some(handle),
    }
}

/// RAII guard for [`spawn_tool_heartbeat`]. Aborts the heartbeat task
/// on drop so a panicking executor or an early `?`-return at the
/// caller doesn't leak the periodic emission past the tool's
/// lifetime.
pub(super) struct HeartbeatGuard {
    handle: Option<JoinHandle<()>>,
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl AgentLoop {
    /// Process tool call results from one iteration.
    ///
    /// Returns `(results, side_messages, blocked_ids)` where
    /// `side_messages` are warning/build texts that should be embedded into
    /// the `tool_result` user message rather than pushed as separate messages
    /// (which would violate Anthropic's `tool_use/tool_result` adjacency
    /// requirement), and `blocked_ids` tracks which tool calls were
    /// short-circuited by the pre-dispatch chunk guard (for accurate source
    /// labelling in logs).
    pub(crate) async fn process_tool_results(
        &self,
        tool_calls: &[ToolCallInfo],
        executor: &dyn AgentToolExecutor,
        state: &mut LoopState,
        event_tx: Option<&Sender<AgentLoopEvent>>,
    ) -> (Vec<ToolCallResult>, Vec<String>, HashSet<String>) {
        let mut side_messages: Vec<String> = Vec::new();

        // Pre-dispatch chunk guard: short-circuit oversized `write_file`
        // calls before the real executor runs, so the turn never pays
        // for the huge content re-echo. These synthetic errors flow
        // through the `blocked_ids` channel so the source label
        // becomes `blocked` and cache invalidation skips them.
        let (oversized_writes, after_oversized) =
            partition_oversized_writes(tool_calls, &mut side_messages, event_tx);

        let (circling_reads, to_execute) =
            partition_circling_duplicate_reads(&after_oversized, state);

        let blocked_ids: HashSet<String> = oversized_writes
            .iter()
            .chain(circling_reads.iter())
            .map(|r| r.tool_use_id.clone())
            .collect();

        let executed = if to_execute.is_empty() {
            Vec::new()
        } else {
            // Phase 6 of agent-stuck-and-reset: spawn a periodic
            // `progress: tool_running` heartbeat so aura-os's
            // sliding-idle watchdog (and the client-side stuck-stream
            // watchdog) see forward motion during a long tool call
            // and don't trip `turn_timeout` on a turn that is
            // actively working. The guard's `Drop` aborts the
            // heartbeat task as soon as `executor.execute` returns
            // (success, error, or panic — the surrounding `await`
            // still drops the guard before unwinding propagates).
            let _heartbeat = spawn_tool_heartbeat(event_tx, &to_execute, tool_heartbeat_interval());
            executor.execute(&to_execute).await
        };

        let any_write_success = track_tool_effects(
            &to_execute,
            &executed,
            &mut state.result,
            &mut state.exploration_state,
            &mut state.had_any_write,
            &mut state.turn_diff,
            Some(&mut state.repeated_read_tracker),
            Some(&mut state.session_read_paths),
            Some(&mut state.read_after_write_allowances),
        );

        if any_write_success && state.build_cooldown == 0 {
            if let Some(build_text) = run_auto_build(
                &self.config,
                executor,
                &mut state.build_cooldown,
                state.build_baseline.as_ref(),
            )
            .await
            {
                side_messages.push(build_text);
            }
        }

        let mut all_results = oversized_writes;
        all_results.extend(circling_reads);
        all_results.extend(executed);
        (all_results, side_messages, blocked_ids)
    }
}

/// Operator opt-out for the post-`implement_now` hard exploration block.
///
/// The soft `<harness_steering kind="implement_now">` prompt always fires when
/// the gate's preconditions are met (see `prompts::steering::implement_now_gate`).
/// The pre-dispatch tool-result rejection in
/// [`partition_circling_duplicate_reads`] is the stronger guard: on by default
/// once `implement_now` has injected. Set `AURA_AGENT_IMPLEMENT_NOW_BLOCK` to
/// `0` / `false` / `no` / `off` for soft-nudge-only behaviour.
fn implement_now_block_enabled() -> bool {
    !matches!(
        std::env::var("AURA_AGENT_IMPLEMENT_NOW_BLOCK").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// Pre-dispatch exploration block (on by default).
///
/// When [`implement_now_block_enabled`] is true and the loop has injected the
/// one-shot `implement_now` steering without any cumulative file writes,
/// further read/search tool calls are short-circuited with a synthetic error
/// result.
pub(super) fn partition_circling_duplicate_reads(
    tool_calls: &[ToolCallInfo],
    state: &super::LoopState,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>) {
    if !implement_now_block_enabled() || !state.implement_now_injected || state.had_any_file_write {
        return (Vec::new(), tool_calls.to_vec());
    }

    let mut blocked = Vec::new();
    let mut remaining = Vec::with_capacity(tool_calls.len());
    for tool in tool_calls {
        if helpers::is_exploration_tool(&tool.name) {
            blocked.push(ToolCallResult {
                tool_use_id: tool.id.clone(),
                content: "implement_now has already fired after enough exploration. This read/search tool was blocked; the next action must be write_file, edit_file, delete_file, or task_done with no_changes_needed: true and notes explaining why no file changes are required.".to_string(),
                is_error: true,
                kind: aura_core::ToolResultKind::AgentError,
                stop_loop: false,
                file_changes: Vec::new(),
            });
        } else {
            remaining.push(tool.clone());
        }
    }
    (blocked, remaining)
}

/// Pre-dispatch chunk guard for `write_file`.
///
/// Short-circuits any `write_file` call whose `content` field exceeds
/// [`WRITE_FILE_CHUNK_BYTES`]. The returned [`ToolCallResult`] is marked
/// `is_error = true` so the existing `update_cache` / `any_write`
/// detection treats it as NOT a successful write — nothing touches disk,
/// nothing clears the read-only cache. The same message is also pushed
/// as a side-message and emitted through the event stream as
/// [`AgentLoopEvent::Warning`] so it is visible to humans watching the
/// run.
fn partition_oversized_writes(
    tool_calls: &[ToolCallInfo],
    side_messages: &mut Vec<String>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>) {
    let mut oversized = Vec::new();
    let mut remaining = Vec::with_capacity(tool_calls.len());

    for tool in tool_calls {
        if tool.name == "write_file" {
            if let Some(content) = tool.input.get("content").and_then(|v| v.as_str()) {
                if content.len() > WRITE_FILE_CHUNK_BYTES {
                    let path_hint = tool
                        .input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let msg = format!(
                        "`write_file` content of {} bytes exceeds the {}-byte per-turn cap. \
                         Next turn: call `write_file` with only the module-doc + imports + one stub \
                         (≤{} bytes), then use `edit_file` appends for the rest.",
                        content.len(),
                        WRITE_FILE_CHUNK_BYTES,
                        WRITE_FILE_CHUNK_BYTES,
                    );
                    let content_msg = format!("[CHUNK_GUARD] {msg}");
                    warn!(
                        tool_use_id = %tool.id,
                        tool_name = %tool.name,
                        path = path_hint,
                        content_bytes = content.len(),
                        chunk_cap = WRITE_FILE_CHUNK_BYTES,
                        "write_file content exceeds per-turn chunk cap; short-circuiting"
                    );

                    emit_event(event_tx, AgentLoopEvent::Warning(msg.clone()));

                    side_messages.push(msg);
                    oversized.push(ToolCallResult {
                        tool_use_id: tool.id.clone(),
                        content: content_msg,
                        is_error: true,
                        kind: aura_core::ToolResultKind::AgentError,
                        stop_loop: false,
                        file_changes: Vec::new(),
                    });
                    continue;
                }
            }
        }
        remaining.push(tool.clone());
    }

    (oversized, remaining)
}

/// Crate-internal wrapper around [`track_tool_effects`] used by the
/// E.3 streaming pump (`stream_pump::handle_streamed_tool_use`). The
/// pump's tool dispatch path lives outside this file so the
/// detection logic for "successful write happened this batch" is
/// exposed via `pub(super)` rather than inlined into both
/// dispatchers. Same semantics as the private version — separated
/// only to keep the visibility surface honest (Rule 3.1: nothing is
/// `pub` that doesn't have to be).
pub(super) fn track_tool_effects_public(
    to_execute: &[ToolCallInfo],
    executed: &[ToolCallResult],
    result: &mut AgentLoopResult,
    exploration_state: &mut ExplorationState,
    had_any_write: &mut bool,
    turn_diff: &mut super::turn_diff::TurnDiff,
    repeated_read_tracker: Option<&mut crate::prompts::steering::RepeatedReadTracker>,
    session_read_paths: Option<&mut HashSet<PathBuf>>,
    read_after_write_allowances: Option<&mut HashMap<PathBuf, u8>>,
) -> bool {
    track_tool_effects(
        to_execute,
        executed,
        result,
        exploration_state,
        had_any_write,
        turn_diff,
        repeated_read_tracker,
        session_read_paths,
        read_after_write_allowances,
    )
}

fn track_tool_effects(
    to_execute: &[ToolCallInfo],
    executed: &[ToolCallResult],
    result: &mut AgentLoopResult,
    exploration_state: &mut ExplorationState,
    had_any_write: &mut bool,
    turn_diff: &mut super::turn_diff::TurnDiff,
    mut repeated_read_tracker: Option<&mut crate::prompts::steering::RepeatedReadTracker>,
    mut session_read_paths: Option<&mut HashSet<PathBuf>>,
    mut read_after_write_allowances: Option<&mut HashMap<PathBuf, u8>>,
) -> bool {
    use super::turn_diff::TurnDiff;
    use crate::types::FileChangeKind;

    fn record_into_turn_diff(turn_diff: &mut TurnDiff, change: &crate::types::FileChange) {
        let path = std::path::PathBuf::from(&change.path);
        match change.kind {
            FileChangeKind::Create => turn_diff.record_create(path),
            FileChangeKind::Modify => {
                // No raw byte count is plumbed through `FileChange`
                // today; the codex tracker uses an approximation too.
                // Use `lines_added` as a cheap proxy so the count is
                // monotonic and non-zero for typical edits.
                let bytes = change.lines_added as usize;
                turn_diff.record_modify(path, bytes);
            }
            FileChangeKind::Delete => turn_diff.record_delete(path),
        }
    }

    let mut any_write_success = false;

    for exec_result in executed {
        let Some(tool) = to_execute.iter().find(|t| t.id == exec_result.tool_use_id) else {
            continue;
        };

        if helpers::is_exploration_tool(&tool.name) {
            exploration_state.count += 1;
            if !exec_result.is_error {
                if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                    let path_buf = PathBuf::from(path);
                    turn_diff.record_read(path_buf.clone());
                    if let Some(cache) = session_read_paths.as_deref_mut() {
                        cache.insert(path_buf);
                    }
                    if let Some(allowances) = read_after_write_allowances.as_deref_mut() {
                        let key = PathBuf::from(path);
                        let mut exhausted = false;
                        if let Some(remaining) = allowances.get_mut(&key) {
                            *remaining = remaining.saturating_sub(1);
                            exhausted = *remaining == 0;
                        }
                        if exhausted {
                            allowances.remove(&key);
                        }
                    }
                }
                if tool.name == "read_file" {
                    if let Some(tracker) = repeated_read_tracker.as_deref_mut() {
                        let hash =
                            super::tool_execution::content_hash_hex(exec_result.content.as_bytes());
                        tracker.record(&hash);
                    }
                }
            }
        }

        if helpers::is_write_tool(&tool.name) {
            // Skip failed write attempts entirely: the tool layer
            // already returned an error to the model, and the
            // codex-parity loop no longer harvests per-iteration
            // failed-write telemetry for continuation injection.
            if exec_result.is_error {
                continue;
            }

            let path_arg = tool.input.get("path").and_then(|v| v.as_str());
            if let Some(path) = path_arg {
                let path_buf = PathBuf::from(path);
                if let Some(cache) = session_read_paths.as_deref_mut() {
                    cache.remove(&path_buf);
                }
                if let Some(allowances) = read_after_write_allowances.as_deref_mut() {
                    allowances.insert(path_buf, READS_AFTER_WRITE_ALLOWANCE);
                }
                any_write_success = true;
                *had_any_write = true;
                for change in &exec_result.file_changes {
                    result.record_file_change(change.clone());
                    record_into_turn_diff(turn_diff, change);
                }
            } else if !exec_result.file_changes.is_empty() {
                // Multi-file write fallback: the tool has no single
                // `path` argument but the result carries one
                // `FileChange` per touched file. Each change is
                // treated as a successful write so the file-change
                // journal and Phase B's `had_any_file_write` latch
                // light up the same way they do for the granular
                // write tools.
                for change in &exec_result.file_changes {
                    let path_buf = PathBuf::from(&change.path);
                    if let Some(cache) = session_read_paths.as_deref_mut() {
                        cache.remove(&path_buf);
                    }
                    if let Some(allowances) = read_after_write_allowances.as_deref_mut() {
                        allowances.insert(path_buf, READS_AFTER_WRITE_ALLOWANCE);
                    }
                    result.record_file_change(change.clone());
                    record_into_turn_diff(turn_diff, change);
                }
                any_write_success = true;
                *had_any_write = true;
            }
        }
    }

    any_write_success
}

async fn run_auto_build(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    build_cooldown: &mut usize,
    build_baseline: Option<&BuildBaseline>,
) -> Option<String> {
    if let Some(build_result) = executor.auto_build_check().await {
        *build_cooldown = config.auto_build_cooldown;
        if !build_result.success {
            let annotated = build_baseline.map_or_else(
                || build_result.output.clone(),
                |baseline| build::annotate_build_output(&build_result.output, baseline),
            );
            return Some(format!(
                "Build check failed with {} error(s):\n\n{annotated}",
                build_result.error_count
            ));
        }
    }
    None
}

/// Crate-internal wrapper around [`run_auto_build`] used by the
/// E.4 streaming pump (`stream_pump::handle_streamed_tool_use`) so
/// the pump path participates in the same dev-loop build feedback
/// as the buffered path. Same semantics — separated only to keep
/// the visibility surface honest (Rule 3.1: `run_auto_build` itself
/// stays private to this module).
pub(super) async fn run_auto_build_public(
    config: &AgentLoopConfig,
    executor: &dyn AgentToolExecutor,
    build_cooldown: &mut usize,
    build_baseline: Option<&BuildBaseline>,
) -> Option<String> {
    run_auto_build(config, executor, build_cooldown, build_baseline).await
}

#[cfg(test)]
mod chunk_guard_tests {
    use super::*;
    use serde_json::json;

    fn mk_tool(id: &str, name: &str, input: serde_json::Value) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    #[test]
    fn write_file_over_chunk_bytes_is_rejected_without_disk_write() {
        // Content size must exceed `WRITE_FILE_CHUNK_BYTES` (32_000
        // after the harness-dev-loop-efficiency relaxation).
        let huge = "x".repeat(33_000);
        let call = mk_tool(
            "toolu_1",
            "write_file",
            json!({"path": "src/big.rs", "content": huge}),
        );
        eprintln!(
            "DEBUG: content_len={}, chunk_cap={}",
            huge.len(),
            WRITE_FILE_CHUNK_BYTES
        );
        eprintln!(
            "DEBUG: tool.name={}, input_content_len={:?}",
            call.name,
            call.input
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
        );
        let mut side_messages: Vec<String> = Vec::new();
        let (oversized, remaining) =
            partition_oversized_writes(std::slice::from_ref(&call), &mut side_messages, None);
        eprintln!(
            "DEBUG: oversized={}, remaining={}",
            oversized.len(),
            remaining.len()
        );

        assert_eq!(
            oversized.len(),
            1,
            "one oversized write should short-circuit"
        );
        assert_eq!(oversized[0].tool_use_id, "toolu_1");
        assert!(
            oversized[0].is_error,
            "synthetic tool_result must be is_error=true so cache invalidation skips it"
        );
        assert!(
            oversized[0].file_changes.is_empty(),
            "chunk guard must NOT record any file changes (nothing hit disk)"
        );
        assert!(
            oversized[0].content.contains("edit_file"),
            "synthetic content should name edit_file in the recovery hint"
        );
        assert!(
            oversized[0].content.contains("32000"),
            "synthetic content should reference the byte cap"
        );
        assert!(
            remaining.is_empty(),
            "oversized write should NOT be forwarded to the executor"
        );
        assert_eq!(
            side_messages.len(),
            1,
            "a warning side-message should be queued for the next user turn"
        );
    }

    #[test]
    fn write_file_under_chunk_bytes_proceeds() {
        let small = "y".repeat(2_000);
        let call = mk_tool(
            "toolu_2",
            "write_file",
            json!({"path": "src/small.rs", "content": small}),
        );
        let mut side_messages: Vec<String> = Vec::new();
        let (oversized, remaining) =
            partition_oversized_writes(std::slice::from_ref(&call), &mut side_messages, None);

        assert!(
            oversized.is_empty(),
            "under-threshold writes must pass through unchanged"
        );
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "toolu_2");
        assert!(side_messages.is_empty());
    }

    #[test]
    fn chunk_guard_ignores_non_write_tools() {
        let big_arg = "z".repeat(10_000);
        let call = mk_tool("toolu_3", "search_code", json!({"pattern": big_arg}));
        let mut side_messages: Vec<String> = Vec::new();
        let (oversized, remaining) =
            partition_oversized_writes(std::slice::from_ref(&call), &mut side_messages, None);
        assert!(oversized.is_empty());
        assert_eq!(remaining.len(), 1);
    }
}

#[cfg(test)]
mod track_tool_effects_tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, OnceLock};

    /// Serialize tests that read or mutate `AURA_AGENT_IMPLEMENT_NOW_BLOCK`.
    /// `partition_circling_duplicate_reads` reads the same env var, so any
    /// test exercising it needs to coordinate to keep parallel-test runs
    /// deterministic.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Set `AURA_AGENT_IMPLEMENT_NOW_BLOCK` to `value` for the duration of a
    /// scope and restore the previous value on drop.
    struct EnvVarOverride {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarOverride {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarOverride {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn mk_read_tool(id: &str, path: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: json!({"path": path}),
        }
    }

    fn mk_read_result(tool_use_id: &str) -> ToolCallResult {
        ToolCallResult {
            tool_use_id: tool_use_id.to_string(),
            content: "ok".to_string(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
        }
    }

    fn mk_write_tool(id: &str, name: &str, path: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: json!({"path": path}),
        }
    }

    fn mk_write_result(tool_use_id: &str, path: &str) -> ToolCallResult {
        ToolCallResult {
            tool_use_id: tool_use_id.to_string(),
            content: "ok".to_string(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: vec![crate::types::FileChange {
                path: path.to_string(),
                kind: crate::types::FileChangeKind::Modify,
                lines_added: 1,
                lines_removed: 0,
            }],
        }
    }

    #[test]
    fn implement_now_block_is_disabled_when_env_opt_out() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _override = EnvVarOverride::set("AURA_AGENT_IMPLEMENT_NOW_BLOCK", "off");

        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new_for_tests(&config, vec![]);
        state.implement_now_injected = true;

        let calls = vec![
            mk_read_tool("toolu_read", "src/lib.rs"),
            ToolCallInfo {
                id: "toolu_search".to_string(),
                name: "search_code".to_string(),
                input: json!({"pattern": "Dedupe"}),
            },
        ];

        let (blocked, remaining) = partition_circling_duplicate_reads(&calls, &state);

        assert!(
            blocked.is_empty(),
            "opt-out: hard block must not fire when AURA_AGENT_IMPLEMENT_NOW_BLOCK=off"
        );
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn implement_now_blocks_exploration_until_a_write_lands() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _override = EnvVarOverride::set("AURA_AGENT_IMPLEMENT_NOW_BLOCK", "");

        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new_for_tests(&config, vec![]);
        state.implement_now_injected = true;

        let calls = vec![
            mk_read_tool("toolu_read", "src/lib.rs"),
            ToolCallInfo {
                id: "toolu_search".to_string(),
                name: "search_code".to_string(),
                input: json!({"pattern": "Dedupe"}),
            },
            mk_write_tool("toolu_write", "edit_file", "src/lib.rs"),
            ToolCallInfo {
                id: "toolu_done".to_string(),
                name: "task_done".to_string(),
                input: json!({"notes": "done"}),
            },
        ];

        let (blocked, remaining) = partition_circling_duplicate_reads(&calls, &state);

        assert_eq!(blocked.len(), 2);
        assert!(blocked.iter().all(|r| r.is_error));
        assert!(blocked
            .iter()
            .all(|r| r.content.contains("implement_now has already fired")));
        assert_eq!(
            remaining
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["edit_file", "task_done"]
        );
    }

    #[test]
    fn implement_now_block_stops_after_first_successful_file_write() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _override = EnvVarOverride::set("AURA_AGENT_IMPLEMENT_NOW_BLOCK", "");

        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new_for_tests(&config, vec![]);
        state.implement_now_injected = true;
        state.had_any_file_write = true;

        let calls = vec![mk_read_tool("toolu_read", "src/lib.rs")];
        let (blocked, remaining) = partition_circling_duplicate_reads(&calls, &state);

        assert!(blocked.is_empty());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "read_file");
    }

    /// Pin that `track_tool_effects` increments the exploration
    /// telemetry counter on every successful exploration call. The
    /// counter survives for telemetry / future re-introduction of a
    /// real cap.
    #[test]
    fn exploration_count_is_incremented_by_track_tool_effects() {
        const CALLS: usize = 12;

        let mut exploration_state = ExplorationState::default();
        let mut result = AgentLoopResult::default();
        let mut had_any_write = false;
        let mut turn_diff = super::super::turn_diff::TurnDiff::default();

        for i in 0..CALLS {
            let tool_id = format!("toolu_explore_{i}");
            let path = format!("src/file_{i}.rs");
            let to_execute = vec![mk_read_tool(&tool_id, &path)];
            let executed = vec![mk_read_result(&tool_id)];
            track_tool_effects(
                &to_execute,
                &executed,
                &mut result,
                &mut exploration_state,
                &mut had_any_write,
                &mut turn_diff,
                None,
                None,
                None,
            );
        }

        assert_eq!(exploration_state.count, CALLS);
        assert!(!had_any_write, "no writes were issued in this test");
        assert!(
            turn_diff.writes.is_empty(),
            "read_file calls must not count as writes in the turn diff"
        );
        assert_eq!(turn_diff.read_paths.len(), CALLS);
    }

    #[test]
    fn successful_write_removes_path_from_session_read_cache() {
        let mut exploration_state = ExplorationState::default();
        let mut result = AgentLoopResult::default();
        let mut had_any_write = false;
        let mut turn_diff = super::super::turn_diff::TurnDiff::default();
        let mut session_read_paths = HashSet::from([PathBuf::from("src/inbox.rs")]);
        let mut read_after_write_allowances = HashMap::new();

        let to_execute = vec![mk_write_tool("toolu_write", "edit_file", "src/inbox.rs")];
        let executed = vec![mk_write_result("toolu_write", "src/inbox.rs")];

        let any_success = track_tool_effects(
            &to_execute,
            &executed,
            &mut result,
            &mut exploration_state,
            &mut had_any_write,
            &mut turn_diff,
            None,
            Some(&mut session_read_paths),
            Some(&mut read_after_write_allowances),
        );

        assert!(any_success);
        assert!(had_any_write);
        assert!(
            !session_read_paths.contains(&PathBuf::from("src/inbox.rs")),
            "writing a file must allow one fresh re-read of that path"
        );
        assert_eq!(
            read_after_write_allowances.get(&PathBuf::from("src/inbox.rs")),
            Some(&READS_AFTER_WRITE_ALLOWANCE)
        );
    }
}
