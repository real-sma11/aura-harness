//! Inner tool-execution pipeline.
//!
//! `super::tool_execution::handle_tool_use` is the outer dispatch step
//! for `StopReason::ToolUse` (cache lookup, event emission, termination
//! checks). When that step has tool calls left to actually run, it
//! delegates here, to [`AgentLoop::process_tool_results`], which is the
//! pipeline that:
//!
//! 1. Pre-dispatch chunk guard for oversized `write_file` calls.
//! 2. Blocking detection (`detect_all_blocked`) + read-guard limits.
//! 3. Hands the survivors to the [`AgentToolExecutor`].
//! 4. Tracks effects (writes / exploration / blocking_ctx) and stalls.
//! 5. Optional auto-build after a successful write.
//!
//! Renamed from `tool_processing` in Phase 4: the old name was
//! confusingly close to `tool_execution`, even though this module sits
//! inside that one. "Pipeline" makes the multi-stage flow explicit and
//! preserves the outer/inner split between the two files.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::blocking::detection::{detect_all_blocked, BlockingContext};
use crate::blocking::stall::StallDetector;
use crate::budget::ExplorationState;
use crate::build;
use crate::constants::WRITE_FILE_CHUNK_BYTES;
use crate::events::{AgentLoopEvent, DebugEvent};
use crate::helpers;
use crate::read_guard::ReadGuardState;
use crate::types::{
    AgentLoopResult, AgentToolExecutor, BuildBaseline, ToolCallInfo, ToolCallResult,
};
use chrono::Utc;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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

/// Build synthetic `tool_result` blocks for tool calls aborted by
/// cancellation. Each result is marked `is_error: true` (so the cache
/// invalidation path treats them as failures) and `stop_loop: true` so
/// `check_termination_conditions` breaks the agent loop on the first
/// cancelled result. The synthetic content is a stable, machine-
/// readable marker (`[CANCELLED]`) so downstream log scrapers can
/// distinguish user-initiated cancellation from a tool returning an
/// error of its own.
///
/// Anthropic requires every assistant `tool_use` block to be paired
/// with a `tool_result` block in the next user message; emitting
/// synthetic results for the entire `to_execute` batch keeps that
/// adjacency intact even though the loop is about to break and the
/// resulting messages will not be re-sent to the model.
fn synthesize_cancelled_results(tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
    tool_calls
        .iter()
        .map(|tc| ToolCallResult {
            tool_use_id: tc.id.clone(),
            content: "[CANCELLED] Tool call aborted by user stop request.".to_string(),
            is_error: true,
            kind: aura_core::ToolResultKind::AgentError,
            stop_loop: true,
            file_changes: Vec::new(),
        })
        .collect()
}

impl AgentLoop {
    /// Process tool call results from one iteration.
    ///
    /// Returns `(results, side_messages, is_stalled, blocked_ids)` where
    /// `side_messages` are warning/build texts that should be embedded into
    /// the `tool_result` user message rather than pushed as separate messages
    /// (which would violate Anthropic's `tool_use/tool_result` adjacency
    /// requirement), and `blocked_ids` tracks which tool calls were blocked
    /// by detection policy (for accurate source labelling in logs).
    pub(crate) async fn process_tool_results(
        &self,
        tool_calls: &[ToolCallInfo],
        executor: &dyn AgentToolExecutor,
        state: &mut LoopState,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
    ) -> (
        Vec<ToolCallResult>,
        Vec<String>,
        bool,
        HashSet<String>,
        bool,
    ) {
        let mut side_messages: Vec<String> = Vec::new();

        // Pre-dispatch chunk guard: short-circuit oversized `write_file`
        // calls before the real executor runs, so the turn never pays
        // for the huge content re-echo. These synthetic errors flow
        // through the same `blocked_ids` channel so the source label
        // becomes `blocked` and cache invalidation skips them.
        let (oversized_writes, post_chunk_calls) =
            partition_oversized_writes(tool_calls, &mut side_messages, event_tx);

        let (blocked_results, to_execute, saw_empty_path_block) = partition_blocked(
            &post_chunk_calls,
            &state.blocking_ctx,
            &state.read_guard,
            &mut side_messages,
            event_tx,
        );

        let blocked_ids: HashSet<String> = oversized_writes
            .iter()
            .chain(blocked_results.iter())
            .map(|r| r.tool_use_id.clone())
            .collect();

        let executed = if to_execute.is_empty() {
            Vec::new()
        } else if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
            // Stop fired between `call_model` returning and tool dispatch;
            // never start a new tool batch after the user pressed Stop.
            // Synthesize cancelled `tool_result` blocks with `stop_loop:
            // true` so `check_termination_conditions` breaks the agent
            // loop on the first cancelled result instead of pretending
            // the tools were skipped (which would let the loop ask the
            // model for more work on the next iteration).
            info!(
                tool_count = to_execute.len(),
                "Cancellation observed before tool dispatch; synthesizing cancelled results"
            );
            synthesize_cancelled_results(&to_execute)
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
            // Race the executor with cancellation so a long-running tool
            // (e.g. `run_command`, `cargo build`) does not block stop
            // for minutes. When cancellation wins, the executor future
            // is dropped and we synthesize cancelled `tool_result`
            // blocks with `stop_loop: true` so the loop breaks on the
            // next `check_termination_conditions`. Note: aborting the
            // future does NOT kill child processes spawned by the
            // executor — that is the executor's responsibility (e.g.
            // tokio `Command` with `kill_on_drop(true)`). The
            // contract here is "stop scheduling new tool work and
            // surface the cancel to the agent loop ASAP"; a leaked
            // subprocess is preferable to a Stop button that takes
            // minutes to register.
            match cancellation_token {
                Some(token) => {
                    tokio::select! {
                        biased;
                        () = token.cancelled() => {
                            info!(
                                tool_count = to_execute.len(),
                                "Cancellation observed during tool execution; aborting in-flight tools"
                            );
                            synthesize_cancelled_results(&to_execute)
                        }
                        results = executor.execute(&to_execute) => results,
                    }
                }
                None => executor.execute(&to_execute).await,
            }
        };

        let any_write_success = track_tool_effects(
            &to_execute,
            &executed,
            &mut state.result,
            &mut state.blocking_ctx,
            &mut state.read_guard,
            &mut state.exploration_state,
            &mut state.had_any_write,
        );

        let stalled = check_stall_detection(&mut state.stall_detector, &to_execute, &executed);

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

        if any_write_success {
            state.blocking_ctx.exploration_allowance += 2;
        }

        let mut all_results = oversized_writes;
        all_results.extend(blocked_results);
        all_results.extend(executed);
        (
            all_results,
            side_messages,
            stalled,
            blocked_ids,
            saw_empty_path_block,
        )
    }
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

fn partition_blocked(
    tool_calls: &[ToolCallInfo],
    blocking_ctx: &BlockingContext,
    read_guard: &ReadGuardState,
    side_messages: &mut Vec<String>,
    event_tx: Option<&Sender<AgentLoopEvent>>,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>, bool) {
    let mut blocked = Vec::new();
    let mut to_execute = Vec::new();
    let mut saw_empty_path_block = false;

    for tool in tool_calls {
        let check = detect_all_blocked(tool, blocking_ctx, read_guard);
        if check.blocked {
            let msg = check
                .recovery_message
                .unwrap_or_else(|| "Blocked".to_string());
            let path_hint = tool
                .input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_empty_path_write =
                helpers::is_write_tool(&tool.name) && path_hint.trim().is_empty();
            if is_empty_path_write {
                saw_empty_path_block = true;
            }
            let blocked_content = format!("[BLOCKED] {msg}");
            warn!(
                tool_use_id = %tool.id,
                tool_name = %tool.name,
                path = path_hint,
                reason = %msg,
                "Tool call blocked by detection policy"
            );

            // Route the blocker into the observability stream so the
            // aura-os loop-log picks it up on `blockers.jsonl`. `kind`
            // is inferred from the kind of tool: writes → duplicate
            // writes (the only blocker category at the moment), reads
            // → read_required, everything else → policy.
            let kind = if helpers::is_write_tool(&tool.name) {
                "duplicate_write"
            } else if helpers::is_exploration_tool(&tool.name) {
                "read_required"
            } else {
                "policy"
            };
            let path = (!path_hint.is_empty()).then(|| path_hint.to_string());
            emit_event(
                event_tx,
                AgentLoopEvent::Debug(DebugEvent::Blocker {
                    timestamp: Utc::now(),
                    kind: kind.to_string(),
                    path,
                    message: blocked_content.clone(),
                    task_id: None,
                }),
            );

            side_messages.push(msg.clone());
            blocked.push(ToolCallResult {
                tool_use_id: tool.id.clone(),
                content: blocked_content,
                is_error: true,
                kind: aura_core::ToolResultKind::AgentError,
                stop_loop: false,
                file_changes: Vec::new(),
            });
        } else {
            to_execute.push(tool.clone());
        }
    }

    (blocked, to_execute, saw_empty_path_block)
}

fn track_tool_effects(
    to_execute: &[ToolCallInfo],
    executed: &[ToolCallResult],
    result: &mut AgentLoopResult,
    blocking_ctx: &mut BlockingContext,
    read_guard: &mut ReadGuardState,
    exploration_state: &mut ExplorationState,
    had_any_write: &mut bool,
) -> bool {
    let mut any_write_success = false;

    for exec_result in executed {
        let Some(tool) = to_execute.iter().find(|t| t.id == exec_result.tool_use_id) else {
            continue;
        };

        if helpers::is_exploration_tool(&tool.name) {
            exploration_state.count += 1;
            if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                if tool.input.get("start_line").is_some() {
                    read_guard.record_range_read(path);
                } else {
                    read_guard.record_full_read(path);
                }
                if tool.name == "read_file" && !exec_result.is_error {
                    blocking_ctx.on_read_path(path);
                }
            }
        }

        if helpers::is_write_tool(&tool.name) {
            if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                if exec_result.is_error {
                    blocking_ctx.on_write_failure(path);
                } else {
                    blocking_ctx.on_write_success(path, read_guard);
                    any_write_success = true;
                    *had_any_write = true;
                    for change in &exec_result.file_changes {
                        result.record_file_change(change.clone());
                    }
                }
            } else if exec_result.is_error {
                blocking_ctx.on_malformed_write();
            }
        }

        if crate::constants::COMMAND_TOOLS.contains(&tool.name.as_str()) {
            blocking_ctx.on_command_result(!exec_result.is_error);
        }
    }

    any_write_success
}

fn check_stall_detection(
    stall_detector: &mut StallDetector,
    to_execute: &[ToolCallInfo],
    executed: &[ToolCallResult],
) -> bool {
    let mut write_targets = HashSet::new();
    let mut any_write_success = false;
    let mut writes_attempted = false;

    for exec_result in executed {
        if let Some(tool) = to_execute.iter().find(|t| t.id == exec_result.tool_use_id) {
            if helpers::is_write_tool(&tool.name) {
                writes_attempted = true;
                if let Some(path) = tool.input.get("path").and_then(|v| v.as_str()) {
                    write_targets.insert(path.to_string());
                    if !exec_result.is_error {
                        any_write_success = true;
                    }
                }
            }
        }
    }

    let stalled = stall_detector.update(&write_targets, any_write_success, writes_attempted);
    if stalled {
        warn!(
            streak = stall_detector.streak(),
            "Stall detected: same write targets failing repeatedly"
        );
    }
    stalled
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
        let huge = "x".repeat(13_000);
        let call = mk_tool(
            "toolu_1",
            "write_file",
            json!({"path": "src/big.rs", "content": huge}),
        );
        let mut side_messages: Vec<String> = Vec::new();
        let (oversized, remaining) =
            partition_oversized_writes(std::slice::from_ref(&call), &mut side_messages, None);

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
            oversized[0].content.contains("12000"),
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
