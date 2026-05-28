//! Line-buffered SSE parsing primitives.
//!
//! Two free functions:
//!
//! - [`pop_event_block`] — given a mutable buffer string, removes and
//!   returns the next complete SSE event block (terminated by `\n\n`
//!   or `\r\n\r\n`). Returns `None` when the buffer is incomplete.
//!   Used by the [`SseStream`] state machine to chunk incoming bytes.
//! - [`parse_sse_event`] — given one event block string, splits its
//!   `event:` / `data:` lines, decodes the JSON payload, and forwards
//!   the result to [`super::event::sse_event_to_stream_event`] for
//!   wire-protocol translation. Handles `event: ping` and malformed
//!   JSON specially (both surface as a `StreamEvent` directly).
//!
//! The line-splitting and the wire-protocol mapping are split across
//! `parse` and `event` so each can be unit-tested in isolation.
//!
//! [`SseStream`]: super::state::SseStream

use super::super::api_types::SseEvent;
use super::event::sse_event_to_stream_event;
use crate::StreamEvent;

/// Hard cap on the SSE buffer size; exceeded buffers are turned into
/// a `ReasonerError::Internal` by the [`SseStream`] state machine to
/// guard against runaway upstream payloads.
///
/// [`SseStream`]: super::state::SseStream
pub(super) const MAX_SSE_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Pop the next complete SSE event block from `buffer`.
///
/// Returns `Some(event_string)` and advances `buffer` past the
/// delimiter when a complete block is available. Returns `None`
/// when the buffer doesn't yet contain a `\n\n` (or `\r\n\r\n`)
/// boundary — the caller (typically [`SseStream`]) should pull more
/// bytes and try again.
///
/// [`SseStream`]: super::state::SseStream
pub(super) fn pop_event_block(buffer: &mut String) -> Option<String> {
    let event_end = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n"))?;
    let event_str = buffer[..event_end].to_string();

    let delimiter_len = if buffer[event_end..].starts_with("\r\n\r\n") {
        4
    } else {
        2
    };
    *buffer = buffer[event_end + delimiter_len..].to_string();

    Some(event_str)
}

/// Parse a single SSE event block string into a [`StreamEvent`].
///
/// Returns:
/// - `Some(StreamEvent::Ping)` for `event: ping` blocks.
/// - `Some(StreamEvent::Error)` for malformed JSON payloads (so the
///   caller can surface the failure without aborting the stream).
/// - `Some(StreamEvent::*)` from
///   [`super::event::sse_event_to_stream_event`] for valid Anthropic
///   stream events.
/// - `None` when the block has no `data:` line (an `event:`-only frame
///   that we can safely ignore).
pub(super) fn parse_sse_event(event_str: &str) -> Option<StreamEvent> {
    let mut event_type = None;
    let mut data = None;

    for line in event_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(suffix) = line.strip_prefix("event:") {
            event_type = Some(suffix.trim().to_string());
        } else if let Some(suffix) = line.strip_prefix("data:") {
            data = Some(suffix.trim().to_string());
        }
    }

    let data = data?;

    if event_type.as_deref() == Some("ping") {
        return Some(StreamEvent::Ping);
    }

    let sse_event: SseEvent = match serde_json::from_str(&data) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "malformed SSE JSON payload");
            return Some(StreamEvent::Error {
                message: format!("malformed SSE JSON: {e}"),
                request_id: None,
            });
        }
    };

    Some(sse_event_to_stream_event(sse_event))
}
