//! Unified tool-execution pipeline (Phase 4).
//!
//! Phase 4 collapsed the two pre-existing tool-dispatch entry points
//! (the buffered path's `tool_execution::handle_tool_use` and the
//! pump path's `stream_pump::dispatch::handle_streamed_tool_use`)
//! behind one async function: [`process_tool_results`]. Both paths
//! now run the same chain — circling-read gate, chunk guard,
//! optional executor call (Live batches only), `track_tool_effects`,
//! auto-build, console/event emission, and the trailing
//! `tool_result`-bearing user message push — regardless of which
//! transport produced the batch.
//!
//! The batch is wrapped in [`ToolBatch`]:
//!
//! - [`ToolBatch::Live`] is a fresh `Vec<ToolCallInfo>` that still
//!   needs to be executed. The buffered transport routes here after
//!   `tool_calls(response)`.
//! - [`ToolBatch::PreExecuted`] is `Vec<(ToolCallInfo, ToolCallResult)>`
//!   the pump driver already executed against its
//!   [`futures_util::stream::FuturesOrdered`] drain. The unified
//!   pipeline takes the pre-executed pairs as a pass-through —
//!   `track_tool_effects` / auto-build / message push still run so
//!   the pump path participates in the same telemetry contract.
//!
//! Each call site builds a [`ToolEffectCtx`] (executor, event_tx,
//! cancellation_token) for the small set of per-dispatch parameters
//! the pipeline needs. The larger sampling context lives in
//! [`super::transport::SamplingCtx`] one layer up.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::budget::ExplorationState;
use crate::build;
use crate::console;
use crate::dup_audit;
use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::types::{
    AgentLoopResult, AgentToolExecutor, BuildBaseline, ToolCallInfo, ToolCallResult,
};
use aura_config::{READS_AFTER_WRITE_ALLOWANCE, TOOL_ERROR_PREVIEW_LIMIT, WRITE_FILE_CHUNK_BYTES};
use aura_reasoner::{ContentBlock, Message, ModelResponse, Role, ToolResultContent};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::event_sink::emit as emit_event;
use super::tool_execution::truncate_preview;
use super::{AgentLoop, AgentLoopConfig, LoopState};

/// Resolved heartbeat cadence. Reads
/// `aura_config::agent().tools.heartbeat_interval` (which is
/// `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS`, clamped at
/// `aura_config` boundaries). Same value the aura-os side reads, so
/// the harness emits a heartbeat well inside the server's
/// sliding-idle window (`AURA_TURN_MAX_TIMEOUT_SECS`, default 180s).
pub(crate) fn tool_heartbeat_interval() -> Duration {
    aura_config::agent().tools.heartbeat_interval
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

/// One batch of tool calls handed to [`process_tool_results`].
///
/// The variant tracks whether the batch still needs to be executed
/// against the [`AgentToolExecutor`] (buffered transport) or whether
/// the streaming pump already executed it inside its
/// [`futures_util::stream::FuturesOrdered`] drain (pump transport).
/// The downstream pipeline runs the same chain regardless — the
/// only branch is "spawn the executor + heartbeat" vs "pass results
/// straight through to tracking".
pub(crate) enum ToolBatch {
    /// Buffered transport: fresh tool calls extracted from
    /// [`tool_calls`] that still need execution.
    Live(Vec<ToolCallInfo>),
    /// Pump transport: tool calls already executed inside the
    /// pump driver, paired with their [`ToolCallResult`] in
    /// submission order.
    PreExecuted(Vec<(ToolCallInfo, ToolCallResult)>),
}

impl ToolBatch {
    /// `true` when the batch carries no tool calls.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Live(v) => v.is_empty(),
            Self::PreExecuted(v) => v.is_empty(),
        }
    }
}

/// Per-dispatch context handed to [`process_tool_results`].
///
/// Kept small and borrowed — the bigger sampling-time bundle lives
/// in [`super::transport::SamplingCtx`]. `ToolEffectCtx` carries only
/// the per-batch arguments that the tool pipeline actually needs:
///
/// - `executor`: target for [`AgentToolExecutor::execute`] on
///   [`ToolBatch::Live`] batches and for the post-write
///   [`AgentToolExecutor::auto_build_check`] on either batch kind.
/// - `event_tx`: optional [`AgentLoopEvent`] sink for warnings,
///   per-tool start/completed/result events, and the post-write
///   build console block.
/// - `cancellation_token`: optional [`CancellationToken`] honored by
///   the [`execute_with_cancellation`] wrapper so a mid-batch Stop
///   resolves to [`cancelled_results_for`] synthetics with
///   `stop_loop = true`.
pub(crate) struct ToolEffectCtx<'a> {
    pub(crate) executor: &'a dyn AgentToolExecutor,
    pub(crate) event_tx: Option<&'a Sender<AgentLoopEvent>>,
    pub(crate) cancellation_token: Option<&'a CancellationToken>,
}

/// Result of running [`process_tool_results`].
///
/// Phase 4 collapsed the previously-distinct buffered/pump
/// "should_stop" plumbing into one bool that captures every
/// terminate-loop signal:
///
/// - a non-error `task_done` returning `stop_loop = true`,
/// - any synthesised `[CANCELLED]` tool_result (mid-tool cancel
///   path),
/// - any tool that explicitly signals `stop_loop = true` from the
///   executor side.
///
/// The struct is intentionally one-field so callers can grow it
/// (e.g. Phase 8 may add a `task_completed` flag) without churning
/// every dispatcher.
pub(crate) struct ProcessedToolResults {
    pub(crate) should_stop: bool,
}

/// Pull every [`ContentBlock::ToolUse`] block out of `response`'s
/// assistant message, preserving submission order.
///
/// Phase 4: merged from the previously-duplicated
/// `tool_execution::extract_tool_calls` and
/// `iteration::extract_pending_tools` helpers. Callers that need
/// the `path` argument (e.g. the `max_tokens` synthetic-truncation
/// builder) extract it from each [`ToolCallInfo::input`] directly
/// instead of carrying a parallel `PendingTool` struct.
#[must_use]
pub(crate) fn tool_calls(response: &ModelResponse) -> Vec<ToolCallInfo> {
    response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(ToolCallInfo {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Run the unified post-`ToolUse` pipeline against `batch` and
/// finalise the per-iteration tool effects on `state`.
///
/// Phase 4 keystone: replaces the previously-divergent
/// `tool_execution::handle_tool_use` (buffered) and
/// `stream_pump::dispatch::handle_streamed_tool_use` (pump) dispatch
/// tails. Both call sites now produce a [`ToolBatch`] and route
/// through this function so `track_tool_effects` / auto-build /
/// `dup_audit` / message-push run uniformly regardless of transport.
///
/// The function:
///
/// 1. Builds the unified "tool_calls + all_results" view, executing
///    [`ToolBatch::Live`] batches through the chunk-guard /
///    circling-read / cache / heartbeat / executor chain. For
///    [`ToolBatch::PreExecuted`] batches the pump driver already ran
///    the tools so the pipeline takes the pre-executed pairs as a
///    pass-through.
/// 2. Runs `track_tool_effects` so the cumulative `had_any_write`
///    latch, file-change journal, repeated-read tracker, and
///    session-read cache all light up the same way on both paths.
/// 3. Triggers auto-build when any successful write landed (with
///    `build_cooldown == 0`) so the dev-loop build feedback loop
///    fires regardless of transport.
/// 4. Emits the per-batch console block and per-tool
///    `ToolCallCompleted` / `ToolResult` events.
/// 5. Pushes a single `Role::User` message carrying the
///    `tool_result` blocks (and any side-message context texts) so
///    the Anthropic `tool_use ↔ tool_result` adjacency contract
///    stays intact.
/// 6. Returns whether the sampling loop should break (the union of
///    every `stop_loop = true` signal in the batch plus the
///    `task_done` success handshake).
pub(crate) async fn process_tool_results(
    state: &mut LoopState,
    agent: &AgentLoop,
    batch: ToolBatch,
    ctx: ToolEffectCtx<'_>,
) -> ProcessedToolResults {
    // Stop-reason parity: when the model signals `ToolUse` but emits
    // no actual tool_use blocks (Phase 3 made this reachable for the
    // pump's MaxTokens-without-pending-tools synthesis path too, see
    // `synthesize.rs::derive_stop_reason`), the loop has nothing to
    // dispatch and must break. Pre-Phase-4 the buffered path's
    // `handle_tool_use` returned early in this case via
    // `execute_and_cache_tools` and the pump's `handle_streamed_tool_use`
    // mirrored it with an explicit `if tool_results.is_empty()`
    // bailout. The unified pipeline preserves that contract here so
    // a no-op `ToolUse` does not spin the sampling loop on a
    // never-executing tool batch.
    if batch.is_empty() {
        return ProcessedToolResults { should_stop: true };
    }

    let prepared = match batch {
        ToolBatch::Live(calls) => prepare_live_batch(state, &calls, &ctx).await,
        ToolBatch::PreExecuted(pairs) => prepare_pre_executed_batch(pairs),
    };

    let PreparedBatch {
        tool_calls,
        mut all_results,
        mut side_messages,
        blocked_ids,
        cached_ids,
    } = prepared;

    // Cumulative tracking (steering trackers, file-change journal,
    // session-read cache, read-after-write allowances). The Live
    // path's cached-results subset is included here because
    // `prepare_live_batch` folded cached pairs into `all_results`
    // before tracking ran. The pump driver does the same merge
    // before calling into the pipeline so PreExecuted batches reach
    // tracking with the full set.
    let any_write_success = track_tool_effects(
        &tool_calls,
        &all_results,
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
            &agent.config,
            ctx.executor,
            &mut state.build_cooldown,
            state.build_baseline.as_ref(),
        )
        .await
        {
            side_messages.push(build_text);
        }
    }

    // Phase B latch: any successful write flips both `had_any_write`
    // (per-batch) and `had_any_file_write` (cumulative). The pump
    // path historically duplicated this assignment inside
    // `handle_streamed_tool_use`; the unified pipeline runs it once
    // here regardless of transport.
    if state.had_any_write {
        state.had_any_file_write = true;
    }

    emit_and_log_results(
        ctx.event_tx,
        &tool_calls,
        &all_results,
        &cached_ids,
        &blocked_ids,
    );

    let task_done_success = tool_calls.iter().any(|tc| tc.name == "task_done")
        && all_results.iter().any(|r| !r.is_error && r.stop_loop);
    if task_done_success {
        state.task_done_completed = true;
    }

    for result in all_results
        .iter()
        .filter(|r| r.kind == aura_core::ToolResultKind::CompactionStructural)
    {
        warn!(
            target: "compaction",
            tool_use_id = %result.tool_use_id,
            result_len = result.content.len(),
            "Rejected compacted/redacted tool input without incrementing consecutive errors"
        );
    }

    let should_stop = all_results.iter().any(|r| r.stop_loop);

    // Drain `all_results` into the trailing user message; carry side
    // messages (chunk-guard warnings + auto-build output) as
    // post-tool_result text blocks. Single dup_audit pre/post pair
    // covers both transports now that the message-push lives here.
    let pushed_results = std::mem::take(&mut all_results);
    push_tool_result_message(&mut state.messages, pushed_results, side_messages);

    ProcessedToolResults { should_stop }
}

/// Intermediate view assembled per batch before the common
/// tracking / auto-build / emit / push steps fire. Held by value
/// inside [`process_tool_results`] so the per-batch-kind logic stays
/// in a single struct and the post-prepare chain is identical.
struct PreparedBatch {
    tool_calls: Vec<ToolCallInfo>,
    all_results: Vec<ToolCallResult>,
    side_messages: Vec<String>,
    blocked_ids: HashSet<String>,
    cached_ids: HashSet<String>,
}

async fn prepare_live_batch(
    state: &mut LoopState,
    tool_calls: &[ToolCallInfo],
    ctx: &ToolEffectCtx<'_>,
) -> PreparedBatch {
    if tool_calls.is_empty() {
        return PreparedBatch {
            tool_calls: Vec::new(),
            all_results: Vec::new(),
            side_messages: Vec::new(),
            blocked_ids: HashSet::new(),
            cached_ids: HashSet::new(),
        };
    }

    debug!(
        tool_count = tool_calls.len(),
        "Processing tool_use stop reason"
    );
    for tc in tool_calls {
        debug!(
            tool_use_id = %tc.id,
            tool_name = %tc.name,
            is_write = helpers::is_write_tool(&tc.name),
            "Tool requested by model"
        );
    }

    let mut side_messages: Vec<String> = Vec::new();
    let (circling_reads, cacheable_calls) = partition_circling_duplicate_reads(tool_calls, state);
    let circling_blocked_ids: HashSet<String> = circling_reads
        .iter()
        .map(|r| r.tool_use_id.clone())
        .collect();

    let (cached_results, uncached_calls) =
        super::tool_execution::split_cached(&cacheable_calls, &state.tool_cache);
    let cached_ids: HashSet<String> = cached_results
        .iter()
        .map(|r| r.tool_use_id.clone())
        .collect();

    debug!(
        cached_count = cached_results.len(),
        execute_count = uncached_calls.len(),
        "Resolved cached vs executable tool calls"
    );

    // Chunk guard runs only on the uncached subset — cached entries
    // were vetted on the original execution turn, and circling-blocked
    // entries never reach a write path.
    let (oversized_writes, after_oversized) =
        partition_oversized_writes(&uncached_calls, &mut side_messages, ctx.event_tx);

    let executed = if after_oversized.is_empty() {
        Vec::new()
    } else {
        // Phase 6 of agent-stuck-and-reset: spawn a periodic
        // `progress: tool_running` heartbeat so aura-os's
        // sliding-idle watchdog (and the client-side stuck-stream
        // watchdog) see forward motion during a long tool call
        // and don't trip `turn_timeout` on a turn that is actively
        // working. The guard's `Drop` aborts the heartbeat task as
        // soon as `executor.execute` returns.
        let _heartbeat =
            spawn_tool_heartbeat(ctx.event_tx, &after_oversized, tool_heartbeat_interval());
        execute_with_cancellation(ctx.executor, &after_oversized, ctx.cancellation_token).await
    };

    super::tool_execution::update_cache(&mut state.tool_cache, &after_oversized, &executed);

    let blocked_ids: HashSet<String> = oversized_writes
        .iter()
        .map(|r| r.tool_use_id.clone())
        .chain(circling_blocked_ids)
        .collect();

    // Ordering: circling-blocked first (the model emitted these
    // first conceptually), then cached, then oversized synthetic
    // errors, then live executed. The codex-parity adjacency
    // contract only requires that EVERY `tool_use` has a paired
    // `tool_result`, not that the order matches submission, so this
    // grouping is purely cosmetic for the trailing user message.
    let mut all_results: Vec<ToolCallResult> = circling_reads;
    all_results.extend(cached_results);
    all_results.extend(oversized_writes);
    all_results.extend(executed);

    PreparedBatch {
        tool_calls: tool_calls.to_vec(),
        all_results,
        side_messages,
        blocked_ids,
        cached_ids,
    }
}

fn prepare_pre_executed_batch(pairs: Vec<(ToolCallInfo, ToolCallResult)>) -> PreparedBatch {
    let mut tool_calls: Vec<ToolCallInfo> = Vec::with_capacity(pairs.len());
    let mut all_results: Vec<ToolCallResult> = Vec::with_capacity(pairs.len());
    for (call, result) in pairs {
        tool_calls.push(call);
        all_results.push(result);
    }
    PreparedBatch {
        tool_calls,
        all_results,
        side_messages: Vec::new(),
        // The pump driver knows which entries were cached or
        // circling-blocked but does not currently surface that
        // metadata into the dispatch tail; both sets stay empty so
        // the console block labels everything as `executor`. Phase 7
        // (legacy buffered deletion) can refine this once the
        // buffered path is gone.
        blocked_ids: HashSet::new(),
        cached_ids: HashSet::new(),
    }
}

/// Console + tracing + event emission for a finished tool batch.
///
/// Phase 4: merged from `tool_execution::emit_and_log_results` so
/// both Live and PreExecuted batches go through one log policy. The
/// pump path previously emitted ToolCallCompleted/ToolResult inline
/// in `handle_streamed_tool_use` without the surrounding
/// `console::tools_block` summary; the unified pipeline restores
/// that summary on both transports.
fn emit_and_log_results(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    tool_calls: &[ToolCallInfo],
    all_results: &[ToolCallResult],
    cached_ids: &HashSet<String>,
    blocked_ids: &HashSet<String>,
) {
    console::tools_block(tool_calls, all_results, cached_ids, blocked_ids);

    for r in all_results {
        let tool_name = tool_calls
            .iter()
            .find(|t| t.id == r.tool_use_id)
            .map_or("unknown", |t| t.name.as_str());
        let source = if cached_ids.contains(&r.tool_use_id) {
            "cache"
        } else if blocked_ids.contains(&r.tool_use_id) {
            "blocked"
        } else {
            "executor"
        };
        if r.is_error {
            let preview = truncate_preview(&r.content, TOOL_ERROR_PREVIEW_LIMIT);
            debug!(
                tool_use_id = %r.tool_use_id,
                tool_name = tool_name,
                is_write = helpers::is_write_tool(tool_name),
                is_error = r.is_error,
                stop_loop = r.stop_loop,
                source = source,
                result_len = r.content.len(),
                result_preview = preview.as_str(),
                "Tool call completed"
            );
        } else {
            debug!(
                tool_use_id = %r.tool_use_id,
                tool_name = tool_name,
                is_write = helpers::is_write_tool(tool_name),
                is_error = r.is_error,
                stop_loop = r.stop_loop,
                source = source,
                result_len = r.content.len(),
                "Tool call completed"
            );
        }
    }

    for r in all_results {
        let info = tool_calls.iter().find(|t| t.id == r.tool_use_id);
        let tool_name = info.map_or_else(String::new, |t| t.name.clone());
        emit_event(
            event_tx,
            AgentLoopEvent::ToolCallCompleted {
                tool_use_id: r.tool_use_id.clone(),
                tool_name: tool_name.clone(),
                input: info.map_or(serde_json::Value::Null, |t| t.input.clone()),
                is_error: r.is_error,
            },
        );
        emit_event(
            event_tx,
            AgentLoopEvent::ToolResult {
                tool_use_id: r.tool_use_id.clone(),
                tool_name,
                content: r.content.clone(),
                is_error: r.is_error,
            },
        );
    }
}

/// Push the trailing `Role::User` message carrying every
/// `tool_result` block (and any side-message text blocks).
///
/// Phase 4: absorbed from `tool_execution::push_tool_result_message_with_context`
/// so [`process_tool_results`] owns the entire dup_audit window.
/// Tool-result blocks are emitted before text blocks so the
/// Anthropic `tool_use ↔ tool_result` adjacency rule stays trivially
/// satisfied even when side messages (chunk-guard warnings,
/// auto-build output) ride along.
pub(super) fn push_tool_result_message(
    messages: &mut Vec<Message>,
    results: Vec<ToolCallResult>,
    context_texts: Vec<String>,
) {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for r in results {
        blocks.push(ContentBlock::tool_result(
            &r.tool_use_id,
            ToolResultContent::text(r.content),
            r.is_error,
        ));
    }
    for text in context_texts {
        blocks.push(ContentBlock::Text { text });
    }

    if !blocks.is_empty() {
        dup_audit::audit_tool_result_duplicates(messages, "push_tool_result.pre");
        messages.push(Message::new(Role::User, blocks));
        dup_audit::audit_tool_result_duplicates(messages, "push_tool_result.post");
    }
}

/// Unified stop-reason dispatch.
///
/// Phase 4: collapses the buffered-path `AgentLoop::dispatch_stop_reason`
/// and the pump-path `stream_pump::dispatch::dispatch_streamed_response`
/// into a single function fed by [`ToolBatch`]. Returns `true` when
/// the sampling loop should break.
pub(crate) async fn dispatch(
    agent: &AgentLoop,
    state: &mut LoopState,
    response: &ModelResponse,
    batch: ToolBatch,
    ctx: ToolEffectCtx<'_>,
) -> bool {
    use aura_reasoner::StopReason;
    match response.stop_reason {
        StopReason::EndTurn | StopReason::StopSequence => true,
        StopReason::MaxTokens => {
            !super::iteration::handle_max_tokens(&agent.config, response, state)
        }
        StopReason::ToolUse => {
            process_tool_results(state, agent, batch, ctx)
                .await
                .should_stop
        }
    }
}

/// Synthesise `[CANCELLED]` tool_result blocks for a batch of
/// in-flight tool calls. Used by [`execute_with_cancellation`] when
/// the user-supplied `CancellationToken` fires while the executor is
/// awaiting tool completion.
///
/// Each synthesised result carries:
///
/// - `is_error = true` so cache invalidation skips it and the model
///   sees the result as a failure.
/// - `kind = AgentError` so the existing telemetry / dup-audit
///   layers treat it as a harness-side rejection rather than a tool
///   crash.
/// - `stop_loop = true` so
///   [`super::tool_execution::check_termination_conditions`] breaks
///   the loop on the same iteration the cancellation arrived. This
///   is the cancellation seam called out by
///   `pipeline_cancellation_mid_tool_execution_aborts_loop`: without
///   `stop_loop`, the loop would happily keep iterating and the
///   model would never see the cancellation as a turn boundary.
///
/// The `[CANCELLED]` tag is the contract surface
/// `pipeline_cancellation_mid_tool_execution_aborts_loop` asserts —
/// it satisfies Anthropic's `tool_use ↔ tool_result` adjacency rule
/// (every `tool_use` in the prior assistant message gets a paired
/// `tool_result` block) so we can break cleanly without leaving the
/// transcript in a structurally-invalid state.
pub(super) fn cancelled_results_for(to_execute: &[ToolCallInfo]) -> Vec<ToolCallResult> {
    to_execute
        .iter()
        .map(|tc| ToolCallResult {
            tool_use_id: tc.id.clone(),
            content:
                "[CANCELLED] Tool execution was cancelled by the user before the tool returned."
                    .to_string(),
            is_error: true,
            kind: aura_core::ToolResultKind::AgentError,
            stop_loop: true,
            file_changes: Vec::new(),
        })
        .collect()
}

/// Wrap a single batch of tool execution in a `tokio::select!` on
/// the user-supplied cancellation token so a mid-tool Stop press
/// terminates the loop on the same iteration instead of waiting for
/// the executor to naturally return. When the token has not been
/// supplied (headless / non-cancellable callers) this collapses to
/// `executor.execute(...).await`.
///
/// Pre-fix, the executor call here was an un-`select!`-ed `.await`,
/// so stop fired on the harness side but the loop kept the
/// (potentially multi-minute) tool round-trip alive — see
/// `pipeline_cancellation_mid_tool_execution_aborts_loop` for the
/// regression contract.
async fn execute_with_cancellation(
    executor: &dyn AgentToolExecutor,
    to_execute: &[ToolCallInfo],
    cancellation_token: Option<&CancellationToken>,
) -> Vec<ToolCallResult> {
    let Some(token) = cancellation_token else {
        return executor.execute(to_execute).await;
    };
    tokio::select! {
        biased;
        () = token.cancelled() => cancelled_results_for(to_execute),
        results = executor.execute(to_execute) => results,
    }
}

/// Pre-dispatch exploration block (on by default).
///
/// When `aura_config::agent().steering.implement_now_hard_block` is `true`
/// (set by `AURA_AGENT_IMPLEMENT_NOW_BLOCK`) and the loop has injected the
/// one-shot `implement_now` steering without any cumulative file writes,
/// further read/search tool calls are short-circuited with a synthetic error
/// result.
pub(super) fn partition_circling_duplicate_reads(
    tool_calls: &[ToolCallInfo],
    state: &super::LoopState,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>) {
    let block_enabled = aura_config::agent().steering.implement_now_hard_block;
    if !block_enabled || !state.implement_now_injected || state.had_any_file_write {
        return (Vec::new(), tool_calls.to_vec());
    }

    let mut blocked = Vec::new();
    let mut remaining = Vec::with_capacity(tool_calls.len());
    for tool in tool_calls {
        if helpers::is_exploration_tool(&tool.name) {
            blocked.push(ToolCallResult {
                tool_use_id: tool.id.clone(),
                content: aura_prompts::model_messages::implement_now::IMPLEMENT_NOW_HARD_BLOCK_BODY
                    .to_string(),
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
                    let msg = aura_prompts::model_messages::chunk_guard::render_chunk_guard_body(
                        content.len(),
                        WRITE_FILE_CHUNK_BYTES,
                    );
                    let content_msg = format!(
                        "{tag}{msg}",
                        tag = aura_prompts::model_messages::chunk_guard::CHUNK_GUARD_TAG,
                    );
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

// Phase 8 will fold these per-tracker arguments into a unified
// `TurnSteeringRegistry`; until then the function holds the existing
// shape that the buffered + pump paths both relied on pre-Phase-4.
#[allow(clippy::too_many_arguments)]
fn track_tool_effects(
    to_execute: &[ToolCallInfo],
    executed: &[ToolCallResult],
    result: &mut AgentLoopResult,
    exploration_state: &mut ExplorationState,
    had_any_write: &mut bool,
    turn_diff: &mut super::turn_diff::TurnDiff,
    mut repeated_read_tracker: Option<&mut super::steering::RepeatedReadTracker>,
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

    /// Serialize tests that swap the installed `aura_config` via
    /// `install_for_test` so they cannot race each other on the
    /// `implement_now_hard_block` toggle.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn install_block(enabled: bool) -> aura_config::ConfigGuard {
        let mut cfg = aura_config::current();
        cfg.agent.steering.implement_now_hard_block = enabled;
        aura_config::install_for_test(cfg)
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
    fn implement_now_block_is_disabled_when_config_opt_out() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cfg = install_block(false);

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
            "opt-out: hard block must not fire when implement_now_hard_block=false"
        );
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn implement_now_blocks_exploration_until_a_write_lands() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cfg = install_block(true);

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
        let _cfg = install_block(true);

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
