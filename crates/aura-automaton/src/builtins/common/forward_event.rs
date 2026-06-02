//! Translation layer between `aura_agent::AgentLoopEvent` and the
//! crate-local [`AutomatonEvent`]. Lives in `builtins/common/` because
//! `dev_loop`, `task_run`, and `chat` all spawn the same forwarder
//! pattern from inside their tick.
//!
//! # Drop policy (post-E.4 follow-up)
//!
//! E.4 turned the streaming pump into a per-`OutputItemDone` event
//! source, so a single sampling can now push 5–30+ advisory events
//! (`TextDelta` / `ThinkingDelta` / `ToolStart` / `ToolInputSnapshot`
//! / `ToolCallCompleted` / `ToolResult`) through this forwarder in a
//! burst. Those events are **advisory UI updates**, not protocol
//! messages — chat clients render them live, but the
//! automaton-runtime state machine and downstream observers
//! reconstruct the task's outcome from the **protocol** events
//! (`TaskStarted` / `TaskCompleted` / `TaskFailed` / `LoopFinished`
//! / `Done`) emitted by `dev_loop/tick.rs` and `runtime.rs` via
//! [`crate::TickContext::emit`].
//!
//! Pre-fix, every advisory event that did not fit the
//! `mpsc::Sender<AutomatonEvent>` buffer (or that arrived after the
//! `EventChannel` consumer dropped its receiver) emitted a per-event
//! `WARN!`. Under the E.4 burst this produced 40+ warnings per task
//! and converted a soft backpressure / lifecycle race into perceived
//! catastrophic failure noise. The fix here:
//!
//! - `forward_agent_event` no longer logs; it returns a typed
//!   [`ForwardOutcome`] so the caller can debounce and decide.
//! - The advisory-drain spawn pattern that used to live (copied) in
//!   `dev_loop/tick.rs`, `chat.rs`, and `task_run.rs` is consolidated
//!   into [`spawn_agent_event_forwarder`] which tracks the dropped /
//!   closed counts per task and logs **at most once per closed-state
//!   transition** and **on power-of-two thresholds** for Full drops.
//! - When the outer channel is observed `Closed`, the forwarder
//!   continues to drain the inner agent channel (so the agent loop's
//!   own `try_send` never accumulates backpressure) but stops
//!   attempting to forward, since the receiver is gone for good.
//!
//! Protocol events still flow through [`crate::TickContext::emit`],
//! which keeps its `try_send` + `?`-propagation contract — a closed
//! receiver on a `TaskStarted` / `TaskCompleted` push is still a
//! tick-level failure, so the operator sees the lifecycle error
//! exactly once instead of 40× advisory warnings followed by an
//! eventual tick failure.

use crate::events::AutomatonEvent;

/// Outcome of a single [`forward_agent_event`] call.
///
/// Returned to the caller so the surrounding drain loop can debounce
/// logging, decide whether to keep attempting forwards, or short-
/// circuit when the receiver is gone for good. See module docs for
/// the policy rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// Event was successfully placed on the outer channel.
    Sent,
    /// The agent emitted an event variant we don't currently project
    /// onto [`AutomatonEvent`] (the `_ => return` wildcard arm). No
    /// action needed; not an error.
    Ignored,
    /// `try_send` returned `TrySendError::Full`. The outer consumer is
    /// behind; the advisory event is dropped. Callers should debounce
    /// the log so a normal burst doesn't produce one warning per
    /// dropped event.
    DroppedFull,
    /// `try_send` returned `TrySendError::Closed`. The outer receiver
    /// has been dropped; subsequent forwards on this channel will all
    /// fail. Callers should log this **at most once per channel** and
    /// stop attempting further forwards.
    DroppedClosed,
}

/// Translate an [`aura_agent::AgentLoopEvent`] to the corresponding
/// [`AutomatonEvent`] and attempt a non-blocking [`try_send`].
///
/// Returns [`ForwardOutcome`] describing the result — see the
/// module-level "Drop policy" docs and the [`ForwardOutcome`] enum
/// for why this no longer logs internally.
///
/// [`try_send`]: tokio::sync::mpsc::Sender::try_send
pub fn forward_agent_event(
    tx: &tokio::sync::mpsc::Sender<AutomatonEvent>,
    evt: aura_agent::AgentLoopEvent,
    task_id: Option<&str>,
) -> ForwardOutcome {
    use aura_agent::AgentLoopEvent;
    let task_id_value = || task_id.map(str::to_owned);
    let automaton_event = match evt {
        AgentLoopEvent::TextDelta(text) => AutomatonEvent::TextDelta {
            task_id: task_id_value(),
            text,
        },
        AgentLoopEvent::ThinkingDelta(thinking) => AutomatonEvent::ThinkingDelta {
            task_id: task_id_value(),
            thinking,
        },
        AgentLoopEvent::ToolStart { id, name } => AutomatonEvent::ToolCallStarted {
            task_id: task_id_value(),
            id,
            name,
        },
        AgentLoopEvent::ToolInputSnapshot { id, name, input } => {
            // Partial JSON is expected while a `tool_use` block is
            // still streaming -- forward it with
            // `snapshot_partial: true` so the UI can render an
            // "in flight…" card instead of dropping the event
            // entirely and leaving the card empty. When the JSON
            // parses cleanly we still emit `snapshot_partial: false`
            // so downstream consumers that only care about finished
            // blocks can filter.
            match serde_json::from_str::<serde_json::Value>(&input) {
                Ok(parsed) => AutomatonEvent::ToolCallSnapshot {
                    task_id: task_id_value(),
                    id,
                    name,
                    input: parsed,
                    snapshot_partial: false,
                },
                Err(_) => AutomatonEvent::ToolCallSnapshot {
                    task_id: task_id_value(),
                    id,
                    name,
                    input: serde_json::Value::String(input),
                    snapshot_partial: true,
                },
            }
        }
        AgentLoopEvent::ToolResult {
            tool_use_id,
            tool_name,
            content,
            is_error,
            image: _,
        } => AutomatonEvent::ToolResult {
            task_id: task_id_value(),
            id: tool_use_id,
            name: tool_name,
            result: content,
            is_error,
        },
        // 1:1 projection of the harness's authoritative completion
        // frame. The server's DoD gate in
        // `apps/aura-os-server/src/handlers/dev_loop.rs`
        // (`successful_write_event_path`) counts
        // `tool_call_completed` events with `is_error=false` as file
        // change evidence when populating
        // `CachedTaskOutput::files_changed`. Without this mapping the
        // gate rejects every pure-edit task with "files 0".
        AgentLoopEvent::ToolCallCompleted {
            tool_use_id,
            tool_name,
            input,
            is_error,
        } => AutomatonEvent::ToolCallCompleted {
            task_id: task_id_value(),
            id: tool_use_id,
            name: tool_name,
            input,
            is_error,
        },
        AgentLoopEvent::IterationComplete {
            input_tokens,
            output_tokens,
            ..
        } => AutomatonEvent::TokenUsage {
            task_id: task_id_value(),
            input_tokens,
            output_tokens,
        },
        AgentLoopEvent::Warning(msg) => AutomatonEvent::LogLine { message: msg },
        AgentLoopEvent::Error { message, .. } => AutomatonEvent::Error {
            automaton_id: String::new(),
            message,
        },
        // Per-tool-call streaming retry lifecycle carries the active
        // task id when this forwarder is used by task-run/dev-loop.
        AgentLoopEvent::ToolCallRetrying {
            tool_use_id,
            tool_name,
            attempt,
            max_attempts,
            delay_ms,
            reason,
        } => AutomatonEvent::ToolCallRetrying {
            task_id: task_id.unwrap_or_default().to_string(),
            tool_use_id,
            tool_name,
            attempt,
            max_attempts,
            delay_ms,
            reason,
        },
        AgentLoopEvent::ToolCallFailed {
            tool_use_id,
            tool_name,
            reason,
        } => AutomatonEvent::ToolCallFailed {
            task_id: task_id.unwrap_or_default().to_string(),
            tool_use_id,
            tool_name,
            reason,
        },
        // `debug.*` observability frames pass through verbatim; the
        // `From<DebugEvent>` impl preserves the exact JSON shape the
        // aura-os forwarder routes on (`type: "debug.<kind>"`).
        AgentLoopEvent::Debug(ev) => AutomatonEvent::from(ev),
        _ => return ForwardOutcome::Ignored,
    };
    match tx.try_send(automaton_event) {
        Ok(()) => ForwardOutcome::Sent,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => ForwardOutcome::DroppedFull,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => ForwardOutcome::DroppedClosed,
    }
}

/// Spawn a background task that drains agent-loop events from
/// `inner_rx`, translates each through [`forward_agent_event`], and
/// forwards the result to `outer_tx`.
///
/// Consolidates the previously-duplicated spawn pattern that lived in
/// `dev_loop/tick.rs`, `chat.rs`, and `task_run.rs`. The single
/// implementation makes the post-E.4 advisory-drop policy
/// (debounced full warnings, log-once closed observation, keep
/// draining inner channel on closed-outer) auditable in one place.
///
/// # Lifecycle
///
/// The spawned task exits when `inner_rx` returns `None` (i.e. all
/// inner senders have dropped — typically when the wrapping
/// `runner.execute_*` call returns and its `event_tx` parameter goes
/// out of scope). The task is otherwise unbounded; callers do not
/// need to track the [`tokio::task::JoinHandle`] for cleanup.
///
/// # Logging cadence
///
/// - First `DroppedFull` per task: `tracing::debug!` with the running
///   drop count.
/// - Subsequent `DroppedFull` are logged at power-of-two thresholds
///   (`1, 2, 4, 8, … 1024, 2048, …`). Each log includes the cumulative
///   count so an operator running with `RUST_LOG=...=debug` still
///   gets a periodic backpressure signal without 40k log lines.
/// - First `DroppedClosed` per task: `tracing::debug!` once. After
///   the closed transition the forwarder keeps consuming `inner_rx`
///   but no longer calls `try_send`, so the outer channel sender
///   count doesn't grow and the agent loop's own `emit_event`
///   doesn't observe backpressure from us.
pub fn spawn_agent_event_forwarder(
    outer_tx: tokio::sync::mpsc::Sender<AutomatonEvent>,
    mut inner_rx: tokio::sync::mpsc::Receiver<aura_agent::AgentLoopEvent>,
    task_id: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut dropped_full: u64 = 0;
        let mut outer_closed = false;
        while let Some(evt) = inner_rx.recv().await {
            if outer_closed {
                // Receiver is permanently gone; keep draining the
                // inner channel so the agent loop's `try_send` never
                // backs up, but skip the (futile) outer forward.
                continue;
            }
            match forward_agent_event(&outer_tx, evt, task_id.as_deref()) {
                ForwardOutcome::Sent | ForwardOutcome::Ignored => {}
                ForwardOutcome::DroppedFull => {
                    dropped_full = dropped_full.saturating_add(1);
                    // First miss + every subsequent power-of-two
                    // threshold. Keeps the log readable while still
                    // surfacing a periodic signal for the rare
                    // sustained-overflow case (large burst when the
                    // outer consumer is genuinely lagging).
                    if dropped_full == 1 || dropped_full.is_power_of_two() {
                        tracing::debug!(
                            task_id = task_id.as_deref(),
                            dropped_full,
                            "automaton event channel full; advisory event dropped"
                        );
                    }
                }
                ForwardOutcome::DroppedClosed => {
                    tracing::debug!(
                        task_id = task_id.as_deref(),
                        "automaton event channel closed; remaining advisory events for \
                         this task will be dropped"
                    );
                    outer_closed = true;
                }
            }
        }
    })
}
