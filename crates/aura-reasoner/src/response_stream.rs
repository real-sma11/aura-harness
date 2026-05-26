//! Provider-neutral high-level streaming event surface (Layer E.3).
//!
//! Codex's `ResponseEvent` enum (see [`turn.rs:1747`](
//! https://github.com/.../codex-rs/core/src/session/turn.rs)) is the
//! shape the codex sampling driver consumes from any model provider:
//! it surfaces *completed* top-level items (assistant message, tool
//! use, thinking block) as they become available, rather than raw
//! per-delta wire frames. The agent loop's stream pump consumes this
//! surface so it can spawn a tool future the moment a `tool_use`
//! item finishes, instead of waiting for the whole response to
//! finalize.
//!
//! # Module-level invariants (Rule 13)
//!
//! - **Ordering**: `OutputItemDone` events arrive in the same order
//!   the content blocks appear in the model's final response. Tools
//!   pushed into a downstream `FuturesOrdered` therefore drain in
//!   submission order (the codex `drain_in_flight` FIFO contract).
//! - **Exactly one `Completed`**: every stream that reaches the end
//!   of its model response emits exactly one [`ResponseEvent::Completed`]
//!   before terminating with `None`. A transport-level abort skips the
//!   `Completed` event and surfaces as `Some(Err(StreamError::…))`.
//! - **No `anyhow`**: this is a library crate per Rule 4.2; the
//!   [`StreamError`] enum derives [`thiserror::Error`] and every
//!   variant carries enough context to be actionable in logs.
//! - **No `unwrap()` outside `#[cfg(test)]`**: the adapter constructs
//!   events from already-validated wire frames; any decode failures
//!   are surfaced via [`StreamError::InvalidEvent`] rather than
//!   panicking.

use std::collections::VecDeque;
use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::types::{
    ContentBlock, PartialToolUse, StopReason, StreamAccumulator, StreamEvent, Usage,
};
use crate::{ModelResponse, StreamEventStream};

/// A complete top-level item emitted by the model during streaming.
///
/// Maps 1:1 onto Anthropic's `content_block_*` event family after the
/// adapter has aggregated per-delta frames into a finished block.
#[derive(Debug, Clone)]
pub enum OutputItem {
    /// A finished assistant text block.
    Message {
        /// The accumulated text content of the block.
        text: String,
    },
    /// A finished thinking block (extended thinking).
    Thinking {
        /// The accumulated thinking text.
        thinking: String,
        /// Optional signature for echoing back to the API.
        signature: Option<String>,
    },
    /// A finished tool-use block. The downstream sampling driver
    /// spawns the tool future the moment this variant arrives.
    ToolUse {
        /// Provider-side tool-use id (e.g. `toolu_01…`).
        id: String,
        /// Tool name (e.g. `read_file`).
        name: String,
        /// Decoded tool-input JSON. Parsed once by the adapter so
        /// every downstream consumer sees the same parse result.
        input: Value,
    },
}

/// Recoverable provider/transport failure surfaced inside a
/// [`ResponseEventStream`]. Library crate => `thiserror` (Rule 4.2).
///
/// `Timeout` is emitted by the *agent-side* pump, not by the adapter
/// itself — the adapter has no notion of deadlines. It lives here so
/// the agent loop can route both transport and timeout failures
/// through a single error type when it surfaces them through
/// `AgentError::Stream` (Rule 4.3).
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The underlying transport (HTTP / SSE / mock channel) closed
    /// before the model emitted a [`ResponseEvent::Completed`] frame.
    #[error("transport closed before stream completed: {context}")]
    TransportClosed {
        /// Human-readable context describing where the close happened
        /// (e.g. `"after content_block_start for tool_use"`).
        context: String,
    },
    /// The transport delivered a syntactically-valid event whose
    /// shape was unexpected (e.g. a `tool_use` block whose
    /// `input_json` failed to decode).
    #[error("provider returned invalid event shape: {reason}")]
    InvalidEvent {
        /// Human-readable reason for the rejection.
        reason: String,
    },
    /// The agent-side pump's `stream_event_timeout` elapsed waiting
    /// for the next [`ResponseEvent`]. Carries `elapsed_ms` so the
    /// surfacing surface (CLI, dashboards) can correlate the failure
    /// with the configured boundary policy.
    #[error("stream timed out after {elapsed_ms}ms waiting for next event")]
    Timeout {
        /// Configured boundary timeout in milliseconds.
        elapsed_ms: u64,
    },
    /// The stream aborted while a `tool_use` block was still being
    /// accumulated. Carries the in-flight tool identity so agent
    /// callers can retry the streaming request without losing which
    /// write/edit attempt was interrupted.
    #[error("{reason}")]
    StreamAbortedWithPartial {
        /// Human-readable provider/transport failure context.
        reason: String,
        /// In-flight tool-use captured just before the stream died.
        partial_tool_use: Option<PartialToolUse>,
    },
}

/// Provider-neutral high-level event emitted while the model is
/// streaming. Mirrors codex's `ResponseEvent` enum.
#[derive(Debug)]
pub enum ResponseEvent {
    /// A complete top-level item arrived. The downstream sampling
    /// driver dispatches based on the inner [`OutputItem`] variant
    /// (e.g. spawn a tool future on `ToolUse`).
    OutputItemDone(OutputItem),
    /// Terminal event for this sampling request. `end_turn` is `Some(true)`
    /// when the model signalled it intends to stop, `Some(false)` when
    /// it explicitly signalled more work, and `None` when the provider
    /// did not include the field (legacy / mock paths).
    Completed {
        /// `Some(false)` indicates the model is *not* ending its turn
        /// (further sampling needed). `None` is the default for
        /// providers that don't surface this bit.
        end_turn: Option<bool>,
        /// Per-response usage counters as reported by the provider.
        usage: Usage,
    },
    /// Recoverable provider error surfaced as an in-band event so
    /// the pump can still decide whether to retry or abort. Transport-
    /// level failures arrive as `Stream::Item = Err(StreamError::…)`
    /// instead; both paths flow through `AgentError::Stream`.
    Error(StreamError),
}

/// Boxed `Stream` of [`ResponseEvent`] items used by `ModelProvider`
/// implementations. Mirrors the [`StreamEventStream`] alias for the
/// lower-level per-delta surface.
pub type ResponseEventStream =
    Pin<Box<dyn Stream<Item = Result<ResponseEvent, StreamError>> + Send + 'static>>;

/// Build a `ResponseEventStream` that synthesises events from a
/// fully-buffered [`ModelResponse`].
///
/// The synthesised stream emits one [`ResponseEvent::OutputItemDone`]
/// per content block (text / thinking / tool_use) followed by a
/// terminal [`ResponseEvent::Completed`]. Used as the fallback path
/// inside the default `complete_response_stream` trait impl for
/// providers that have not been wired to emit incremental events yet.
#[must_use]
pub fn response_stream_from_response(response: ModelResponse) -> ResponseEventStream {
    let mut events: Vec<Result<ResponseEvent, StreamError>> = Vec::new();
    for block in response.message.content {
        match block {
            ContentBlock::Text { text } => {
                events.push(Ok(ResponseEvent::OutputItemDone(OutputItem::Message {
                    text,
                })));
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                events.push(Ok(ResponseEvent::OutputItemDone(OutputItem::Thinking {
                    thinking,
                    signature,
                })));
            }
            ContentBlock::ToolUse { id, name, input } => {
                events.push(Ok(ResponseEvent::OutputItemDone(OutputItem::ToolUse {
                    id,
                    name,
                    input,
                })));
            }
            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
        }
    }
    let end_turn = match response.stop_reason {
        StopReason::EndTurn | StopReason::StopSequence => Some(true),
        StopReason::ToolUse | StopReason::MaxTokens => Some(false),
    };
    events.push(Ok(ResponseEvent::Completed {
        end_turn,
        usage: response.usage,
    }));
    Box::pin(futures_util::stream::iter(events))
}

/// Lift a low-level [`StreamEventStream`] into a [`ResponseEventStream`].
///
/// Aggregates per-delta wire frames into completed items: text /
/// thinking deltas accumulate into a single block until the matching
/// `ContentBlockStop` fires, at which point an
/// [`OutputItem`] is emitted. The terminal `MessageStop` is converted
/// into a [`ResponseEvent::Completed`] with the accumulator's
/// observed stop reason and usage counters.
///
/// Transport errors surface as `Some(Err(StreamError::TransportClosed))`
/// at the point they arrive. Invalid JSON in `InputJsonDelta` raises
/// [`StreamError::InvalidEvent`] inside an in-band
/// [`ResponseEvent::Error`] frame so the pump can decide whether to
/// retry or abort (codex parity: provider-level errors do not
/// short-circuit the stream).
#[must_use]
pub fn response_stream_from_event_stream(stream: StreamEventStream) -> ResponseEventStream {
    let state = AdapterState {
        stream,
        accumulator: StreamAccumulator::new(),
        pending: VecDeque::new(),
        completed_emitted: false,
        terminated: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            if state.terminated {
                return None;
            }
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }

                match state.stream.next().await {
                    Some(Ok(event)) => {
                        state.accumulator.process(&event);
                        state.handle_event(&event);
                    }
                    Some(Err(err)) => {
                        state.terminated = true;
                        let err = state.transport_error(err.to_string());
                        return Some((Err(err), state));
                    }
                    None => {
                        state.terminated = true;
                        if let Some(err) = state.closed_with_partial_error() {
                            return Some((Err(err), state));
                        }
                        if !state.completed_emitted {
                            let event = state.synthesize_completed();
                            return Some((Ok(event), state));
                        }
                        return None;
                    }
                }
            }
        },
    ))
}

struct AdapterState {
    stream: StreamEventStream,
    accumulator: StreamAccumulator,
    pending: VecDeque<Result<ResponseEvent, StreamError>>,
    completed_emitted: bool,
    terminated: bool,
}

impl AdapterState {
    fn handle_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ContentBlockStop { .. } => {
                if let Some(item) = self.take_completed_item() {
                    self.pending
                        .push_back(Ok(ResponseEvent::OutputItemDone(item)));
                }
            }
            StreamEvent::MessageStop => {
                let event = self.synthesize_completed();
                self.pending.push_back(Ok(event));
            }
            StreamEvent::Error { message, .. } => {
                let err = self.provider_error(message.clone());
                match err {
                    StreamError::StreamAbortedWithPartial { .. } => {
                        self.pending.push_back(Err(err));
                    }
                    other => {
                        self.pending.push_back(Ok(ResponseEvent::Error(other)));
                    }
                }
            }
            _ => {}
        }
    }

    fn provider_error(&mut self, message: String) -> StreamError {
        if self.accumulator.current_tool_use.is_some() {
            return StreamError::StreamAbortedWithPartial {
                reason: self.format_stream_error_reason(&message),
                partial_tool_use: self.take_partial_tool_use(),
            };
        }
        StreamError::InvalidEvent { reason: message }
    }

    fn transport_error(&mut self, context: String) -> StreamError {
        if self.accumulator.current_tool_use.is_some() {
            return StreamError::StreamAbortedWithPartial {
                reason: self.format_stream_error_reason(&context),
                partial_tool_use: self.take_partial_tool_use(),
            };
        }
        StreamError::TransportClosed { context }
    }

    fn closed_with_partial_error(&mut self) -> Option<StreamError> {
        if self.accumulator.current_tool_use.is_some() {
            Some(StreamError::StreamAbortedWithPartial {
                reason: self.format_stream_error_reason("transport closed before stream completed"),
                partial_tool_use: self.take_partial_tool_use(),
            })
        } else {
            None
        }
    }

    fn take_partial_tool_use(&mut self) -> Option<PartialToolUse> {
        self.accumulator
            .current_tool_use
            .take()
            .map(|pending| PartialToolUse {
                tool_use_id: pending.id,
                tool_name: pending.name,
                partial_json: pending.input_json,
            })
    }

    fn format_stream_error_reason(&self, message: &str) -> String {
        let mut context_parts: Vec<String> = Vec::new();
        if !self.accumulator.model.is_empty() {
            context_parts.push(format!("model={}", self.accumulator.model));
        }
        if !self.accumulator.message_id.is_empty() {
            context_parts.push(format!("msg_id={}", self.accumulator.message_id));
        }
        if let Some(ref req_id) = self.accumulator.provider_request_id {
            if !req_id.is_empty() {
                context_parts.push(format!("request_id={req_id}"));
            }
        }
        let context = if context_parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", context_parts.join(", "))
        };
        format!("stream terminated with error{context}: {message}")
    }

    /// Pull whichever item the accumulator finished last. The
    /// accumulator's `process(ContentBlockStop)` pushed the closing
    /// tool_use onto `tool_uses`; text / thinking blocks live in the
    /// running accumulators instead.
    fn take_completed_item(&mut self) -> Option<OutputItem> {
        if let Some(tool) = self.accumulator.tool_uses.pop() {
            let input: Value = if tool.input_json.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tool.input_json)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": tool.input_json }))
            };
            return Some(OutputItem::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
            });
        }
        if !self.accumulator.thinking_content.is_empty() {
            let thinking = std::mem::take(&mut self.accumulator.thinking_content);
            let signature = self.accumulator.thinking_signature.take();
            return Some(OutputItem::Thinking {
                thinking,
                signature,
            });
        }
        if !self.accumulator.text_content.is_empty() {
            let text = std::mem::take(&mut self.accumulator.text_content);
            return Some(OutputItem::Message { text });
        }
        None
    }

    fn synthesize_completed(&mut self) -> ResponseEvent {
        self.completed_emitted = true;
        let end_turn = self
            .accumulator
            .stop_reason
            .map(|reason| matches!(reason, StopReason::EndTurn | StopReason::StopSequence));
        let usage = Usage {
            input_tokens: self.accumulator.input_tokens,
            output_tokens: self.accumulator.output_tokens,
            cache_creation_input_tokens: self.accumulator.cache_creation_input_tokens,
            cache_read_input_tokens: self.accumulator.cache_read_input_tokens,
        };
        ResponseEvent::Completed { end_turn, usage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ReasonerError;
    use crate::types::{Message, ProviderTrace, Role, StopReason, Usage};

    fn mk_response() -> ModelResponse {
        ModelResponse::new(
            StopReason::ToolUse,
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::Text {
                        text: "hello".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({ "path": "a" }),
                    },
                ],
            ),
            Usage::new(1, 2),
            ProviderTrace::new("test-model", 10),
        )
    }

    #[tokio::test]
    async fn response_stream_from_response_emits_items_then_completed() {
        let response = mk_response();
        let mut stream = response_stream_from_response(response);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.expect("test stream never yields Err"));
        }
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            ResponseEvent::OutputItemDone(OutputItem::Message { text }) if text == "hello"
        ));
        assert!(matches!(
            &events[1],
            ResponseEvent::OutputItemDone(OutputItem::ToolUse { id, name, .. })
                if id == "toolu_1" && name == "read_file"
        ));
        assert!(matches!(
            &events[2],
            ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn response_stream_from_event_stream_aggregates_blocks() {
        use crate::types::StreamContentType;
        let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
            Ok(StreamEvent::MessageStart {
                message_id: "msg".into(),
                model: "test".into(),
                input_tokens: Some(3),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_type: StreamContentType::Text,
            }),
            Ok(StreamEvent::TextDelta {
                text: "hello ".into(),
            }),
            Ok(StreamEvent::TextDelta {
                text: "world".into(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::ContentBlockStart {
                index: 1,
                content_type: StreamContentType::ToolUse {
                    id: "toolu_1".into(),
                    name: "read_file".into(),
                },
            }),
            Ok(StreamEvent::InputJsonDelta {
                partial_json: "{\"path\":\"a\"}".into(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 1 }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                output_tokens: 5,
            }),
            Ok(StreamEvent::MessageStop),
        ];
        let underlying: StreamEventStream = Box::pin(futures_util::stream::iter(events));
        let mut stream = response_stream_from_event_stream(underlying);
        let mut collected = Vec::new();
        while let Some(event) = stream.next().await {
            collected.push(event.expect("test stream never yields Err"));
        }
        assert_eq!(collected.len(), 3);
        assert!(matches!(
            &collected[0],
            ResponseEvent::OutputItemDone(OutputItem::Message { text }) if text == "hello world"
        ));
        assert!(matches!(
            &collected[1],
            ResponseEvent::OutputItemDone(OutputItem::ToolUse { name, .. }) if name == "read_file"
        ));
        assert!(matches!(
            &collected[2],
            ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn response_stream_from_event_stream_propagates_transport_err() {
        use crate::types::StreamContentType;
        let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_type: StreamContentType::Text,
            }),
            Err(ReasonerError::Internal("boom".into())),
        ];
        let underlying: StreamEventStream = Box::pin(futures_util::stream::iter(events));
        let mut stream = response_stream_from_event_stream(underlying);
        let mut got_err = false;
        while let Some(event) = stream.next().await {
            if let Err(StreamError::TransportClosed { .. }) = event {
                got_err = true;
                break;
            }
        }
        assert!(got_err, "transport error must surface as TransportClosed");
    }

    #[tokio::test]
    async fn response_stream_from_event_stream_preserves_partial_tool_abort() {
        use crate::types::StreamContentType;
        let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
            Ok(StreamEvent::MessageStart {
                message_id: "msg_partial".into(),
                model: "test".into(),
                input_tokens: Some(1),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_type: StreamContentType::ToolUse {
                    id: "toolu_partial".into(),
                    name: "write_file".into(),
                },
            }),
            Ok(StreamEvent::InputJsonDelta {
                partial_json: "{\"path\":\"src/".into(),
            }),
            Ok(StreamEvent::Error {
                message: "overloaded_error: upstream flaked".into(),
                request_id: None,
            }),
        ];
        let underlying: StreamEventStream = Box::pin(futures_util::stream::iter(events));
        let mut stream = response_stream_from_event_stream(underlying);

        match stream.next().await.expect("stream should emit abort") {
            Err(StreamError::StreamAbortedWithPartial {
                reason,
                partial_tool_use: Some(partial),
            }) => {
                assert!(reason.contains("overloaded_error"));
                assert_eq!(partial.tool_use_id, "toolu_partial");
                assert_eq!(partial.tool_name, "write_file");
                assert_eq!(partial.partial_json, "{\"path\":\"src/");
            }
            other => panic!("expected partial abort, got {other:?}"),
        }
    }
}
