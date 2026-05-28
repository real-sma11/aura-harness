//! Anthropic SSE event-type translation.
//!
//! Maps the wire-protocol [`SseEvent`] / [`SseDelta`] /
//! [`SseContentBlock`] enums (deserialized via
//! [`super::super::api_types`]) into the public-facing
//! [`crate::StreamEvent`].
//!
//! Keeping this in its own file isolates the translation rules
//! (e.g. how `MessageDelta.stop_reason` strings map onto
//! [`StopReason`] variants, how the Anthropic-shape `error.type`
//! gets folded into the rendered error message) from both the line
//! parser ([`super::parse`]) and the buffer state machine
//! ([`super::state`]).

use super::super::api_types::{SseContentBlock, SseDelta, SseEvent};
use crate::{StopReason, StreamContentType, StreamEvent};

/// Translate a deserialized Anthropic [`SseEvent`] into a
/// [`StreamEvent`] suitable for downstream consumers
/// (`StreamAccumulator`, dev-loop classifier, UI layer).
pub(super) fn sse_event_to_stream_event(sse_event: SseEvent) -> StreamEvent {
    match sse_event {
        SseEvent::MessageStart { message } => StreamEvent::MessageStart {
            message_id: message.id,
            model: message.model,
            input_tokens: message.usage.as_ref().map(|u| u.input_tokens),
            cache_creation_input_tokens: message
                .usage
                .as_ref()
                .and_then(|u| u.cache_creation_input_tokens),
            cache_read_input_tokens: message
                .usage
                .as_ref()
                .and_then(|u| u.cache_read_input_tokens),
        },
        SseEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            let content_type = match content_block {
                SseContentBlock::Text { .. } => StreamContentType::Text,
                SseContentBlock::Thinking { .. } => StreamContentType::Thinking,
                SseContentBlock::ToolUse { id, name } => StreamContentType::ToolUse { id, name },
            };
            StreamEvent::ContentBlockStart {
                index,
                content_type,
            }
        }
        SseEvent::ContentBlockDelta { delta, .. } => match delta {
            SseDelta::Text { text } => StreamEvent::TextDelta { text },
            SseDelta::Thinking { thinking } => StreamEvent::ThinkingDelta { thinking },
            SseDelta::Signature { signature } => StreamEvent::SignatureDelta { signature },
            SseDelta::InputJson { partial_json } => StreamEvent::InputJsonDelta { partial_json },
        },
        SseEvent::ContentBlockStop { index } => StreamEvent::ContentBlockStop { index },
        SseEvent::MessageDelta { delta, usage } => {
            let stop_reason = delta.stop_reason.as_deref().map(|s| match s {
                "tool_use" => StopReason::ToolUse,
                "max_tokens" => StopReason::MaxTokens,
                "stop_sequence" => StopReason::StopSequence,
                _ => StopReason::EndTurn,
            });
            StreamEvent::MessageDelta {
                stop_reason,
                output_tokens: usage.map_or(0, |u| u.output_tokens),
            }
        }
        SseEvent::MessageStop => StreamEvent::MessageStop,
        SseEvent::Ping => StreamEvent::Ping,
        SseEvent::Error { error } => {
            // Preserve the Anthropic-shape `error.type` in the message so
            // downstream classification (retry policy, UI labeling) can
            // distinguish `overloaded_error` from `api_error` / generic
            // `Internal server error`. Proxies often inject a bare
            // `Internal server error` with no `type`, in which case we
            // fall back to the raw message.
            let request_id = error.request_id.clone();
            let message = match error.error_type.as_deref() {
                Some(kind) if !kind.is_empty() => format!("{kind}: {}", error.message),
                _ => error.message,
            };
            StreamEvent::Error {
                message,
                request_id,
            }
        }
    }
}
