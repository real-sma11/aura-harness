//! Unified [`AgentLoopEvent`] sink (Phase 4).
//!
//! Before Phase 4 the agent loop had two divergent emission policies:
//! [`super::streaming::emit`] used `try_send + warn!` (so a full
//! channel logged at WARN level), while
//! [`super::stream_pump::emit_event`] used `try_send + debug!` (so the
//! pump's hot per-`OutputItemDone` path did not spam the WARN
//! cadence). On top of those, `streaming::emit_with_backpressure`
//! awaited [`tokio::sync::mpsc::Sender::send`] to preserve LLM deltas
//! at the cost of blocking the agent loop on a saturated channel —
//! which violates Rule 6.1 (never block the runtime on a downstream
//! consumer) and exposed the loop to forwarder backpressure storms.
//!
//! Phase 4 collapses these into a single policy ([`emit`] below):
//!
//! - `try_send`: never blocks the loop.
//! - `Full` → `tracing::warn!` once per drop (the loop is making
//!   forward progress faster than the consumer can drain).
//! - `Closed` → `tracing::debug!` once per drop (the consumer
//!   already tore down its receiver; this is a lifecycle signal,
//!   not an error).
//!
//! The previous `emit_with_backpressure` is intentionally removed.
//! Per-delta event preservation now relies on a correctly-sized
//! channel (the chat / dev-loop forwarder allocates ample headroom
//! at session start) rather than on backpressure from the loop side.

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;

use crate::events::AgentLoopEvent;

// Phase 4 intentionally lands only the free-function `emit` policy
// below. An `AgentEventSink` owning wrapper around `Sender` is a
// natural Phase 8 follow-up (it will plug into `RunCtx` / `TurnCtx`
// alongside the broader context-struct refactor), but introducing
// it now would mean a wrapper without any caller and pure dead
// code under the codebase's `-D warnings` clippy gates.

/// Best-effort dispatcher for the optional agent event channel.
///
/// Policy:
///
/// - `tx` is `None` → no-op (headless / no-channel callers).
/// - `try_send` succeeds → done.
/// - `try_send` returns `Full` → `warn!` and drop the event.
/// - `try_send` returns `Closed` → `debug!` and drop the event.
///
/// Replaces the pre-Phase-4 divergence between
/// [`super::streaming::emit`] (`warn!` everywhere) and
/// [`super::stream_pump::emit_event`] (`debug!` everywhere). Both
/// modules now re-export this function so the policy is impossible
/// to drift apart again.
pub(crate) fn emit(tx: Option<&Sender<AgentLoopEvent>>, event: AgentLoopEvent) {
    let Some(tx) = tx else {
        return;
    };
    match tx.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            tracing::warn!("agent event channel full; dropping event");
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!("agent event channel closed; dropping event");
        }
    }
}
