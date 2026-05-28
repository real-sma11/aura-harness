use super::content::{ContentBlock, Role};
use super::message::Message;
use super::response::{ModelResponse, ProviderTrace, StopReason, Usage};
use crate::error::ReasonerError;

/// A streaming event from the model provider.
///
/// These events are emitted during streaming completions, allowing
/// real-time display of model output as it's generated.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Start of a new message
    MessageStart {
        /// Message ID from the provider
        message_id: String,
        /// Model being used
        model: String,
        /// Input tokens (from SSE `message_start` usage)
        input_tokens: Option<u64>,
        /// Cache creation input tokens (prompt caching)
        cache_creation_input_tokens: Option<u64>,
        /// Cache read input tokens (prompt caching)
        cache_read_input_tokens: Option<u64>,
    },

    /// Start of a new content block
    ContentBlockStart {
        /// Index of the content block
        index: u32,
        /// Type of content block (text, `tool_use`, thinking)
        content_type: StreamContentType,
    },

    /// Text delta (incremental text)
    TextDelta {
        /// The text chunk
        text: String,
    },

    /// Thinking delta (incremental thinking content)
    ThinkingDelta {
        /// The thinking text chunk
        thinking: String,
    },

    /// Signature delta (for thinking block signatures)
    SignatureDelta {
        /// The signature chunk
        signature: String,
    },

    /// Tool use input delta (incremental JSON)
    InputJsonDelta {
        /// Partial JSON string
        partial_json: String,
    },

    /// End of a content block
    ContentBlockStop {
        /// Index of the content block
        index: u32,
    },

    /// Final message delta with stop reason
    MessageDelta {
        /// Why the model stopped
        stop_reason: Option<StopReason>,
        /// Output tokens used so far
        output_tokens: u64,
    },

    /// Message complete
    MessageStop,

    /// Ping event (keepalive)
    Ping,

    /// Error event
    Error {
        /// Error message
        message: String,
        /// Optional provider / proxy-supplied HTTP request id, pulled
        /// from the SSE error body (some proxies embed
        /// `error.request_id`). This is a fallback; the primary source
        /// of truth is the synthetic [`StreamEvent::HttpMeta`] event
        /// which carries the response-header `x-request-id`.
        #[doc(hidden)]
        request_id: Option<String>,
    },

    /// Synthetic event emitted by the SSE transport on stream open,
    /// before any provider-level events. Carries HTTP-level metadata
    /// (specifically the `x-request-id` response header) that the
    /// provider never emits inside the SSE body and that a failing
    /// stream would otherwise drop. Not part of the Anthropic / OpenAI
    /// wire protocol — consumers that only care about model output
    /// can safely ignore it.
    HttpMeta {
        /// The `x-request-id` (or `request-id`) header value captured
        /// from the streaming response, if the upstream provided one.
        request_id: Option<String>,
    },
}

/// Type of content in a streaming block.
#[derive(Debug, Clone)]
pub enum StreamContentType {
    /// Text content
    Text,
    /// Thinking content (extended thinking)
    Thinking,
    /// Tool use block
    ToolUse {
        /// Tool use ID
        id: String,
        /// Tool name
        name: String,
    },
}

/// Accumulated state from streaming events.
///
/// This is used to build the final `ModelResponse` from streaming events.
#[derive(Debug, Clone, Default)]
pub struct StreamAccumulator {
    /// Message ID
    pub message_id: String,
    /// Model
    pub model: String,
    /// Accumulated text content
    pub text_content: String,
    /// Accumulated thinking content
    pub thinking_content: String,
    /// Signature for the thinking block (required for echoing back to API)
    pub thinking_signature: Option<String>,
    /// Whether we're currently in a thinking block
    pub in_thinking_block: bool,
    /// Accumulated tool uses
    pub tool_uses: Vec<AccumulatedToolUse>,
    /// Current tool use being built
    pub current_tool_use: Option<AccumulatedToolUse>,
    /// Stop reason
    pub stop_reason: Option<StopReason>,
    /// Input tokens
    pub input_tokens: u64,
    /// Output tokens
    pub output_tokens: u64,
    /// Cache creation input tokens (prompt caching)
    pub cache_creation_input_tokens: Option<u64>,
    /// Cache read input tokens (prompt caching)
    pub cache_read_input_tokens: Option<u64>,
    /// Error captured from a `StreamEvent::Error`.
    pub stream_error: Option<String>,
    /// HTTP `x-request-id` captured from the streaming response.
    ///
    /// Populated by the synthetic [`StreamEvent::HttpMeta`] that
    /// `SseStream` yields before the first provider event, and
    /// secondarily from a request_id embedded in an SSE error body
    /// (only when the header-derived value is still `None`). This is
    /// the id operators should use to correlate a failed stream with
    /// provider / router logs — it is distinct from
    /// [`Self::message_id`], which is the Anthropic message id.
    pub provider_request_id: Option<String>,
}

/// Snapshot of an in-flight `tool_use` block at the moment a mid-stream
/// error truncated the response.
///
/// Returned inside [`crate::error::ReasonerError::StreamAbortedWithPartial`]
/// so the agent loop can surface which tool call was interrupted (and on
/// what partial input) when it decides to retry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PartialToolUse {
    /// Provider-side `tool_use` id (e.g. `toolu_01...`). Empty when the
    /// stream died before `content_block_start` landed.
    pub tool_use_id: String,
    /// Tool name (`write_file`, `edit_file`, ...). Empty when unknown.
    pub tool_name: String,
    /// Accumulated tool-input JSON so far. May be empty or truncated
    /// (not parseable on its own).
    pub partial_json: String,
}

/// Tool use being accumulated from streaming events.
#[derive(Debug, Clone, Default)]
pub struct AccumulatedToolUse {
    /// Tool use ID
    pub id: String,
    /// Tool name
    pub name: String,
    /// Accumulated JSON input
    pub input_json: String,
}

impl StreamAccumulator {
    /// Create a new accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a streaming event.
    pub fn process(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::MessageStart {
                message_id,
                model,
                input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            } => {
                self.message_id.clone_from(message_id);
                self.model.clone_from(model);
                if let Some(tokens) = input_tokens {
                    self.input_tokens = *tokens;
                }
                self.cache_creation_input_tokens = *cache_creation_input_tokens;
                self.cache_read_input_tokens = *cache_read_input_tokens;
            }
            StreamEvent::ContentBlockStart { content_type, .. } => match content_type {
                StreamContentType::ToolUse { id, name } => {
                    self.current_tool_use = Some(AccumulatedToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input_json: String::new(),
                    });
                    self.in_thinking_block = false;
                }
                StreamContentType::Thinking => {
                    self.in_thinking_block = true;
                }
                StreamContentType::Text => {
                    self.in_thinking_block = false;
                }
            },
            StreamEvent::TextDelta { text } => {
                self.text_content.push_str(text);
            }
            StreamEvent::ThinkingDelta { thinking } => {
                self.thinking_content.push_str(thinking);
            }
            StreamEvent::SignatureDelta { signature } => {
                if let Some(ref mut sig) = self.thinking_signature {
                    sig.push_str(signature);
                } else {
                    self.thinking_signature = Some(signature.clone());
                }
            }
            StreamEvent::InputJsonDelta { partial_json } => {
                if let Some(tool) = &mut self.current_tool_use {
                    tool.input_json.push_str(partial_json);
                }
            }
            StreamEvent::ContentBlockStop { .. } => {
                if let Some(tool) = self.current_tool_use.take() {
                    self.tool_uses.push(tool);
                }
                self.in_thinking_block = false;
            }
            StreamEvent::MessageDelta {
                stop_reason,
                output_tokens,
            } => {
                self.stop_reason = *stop_reason;
                self.output_tokens = *output_tokens;
            }
            StreamEvent::MessageStop | StreamEvent::Ping => {}
            StreamEvent::Error {
                message,
                request_id,
            } => {
                self.stream_error = Some(message.clone());
                // Body-level request_id only wins when the HTTP header
                // path did not already supply one — the header value
                // also covers the success path and is therefore the
                // source of truth.
                if self.provider_request_id.is_none() {
                    if let Some(id) = request_id {
                        self.provider_request_id = Some(id.clone());
                    }
                }
            }
            StreamEvent::HttpMeta { request_id } => {
                if let Some(id) = request_id {
                    self.provider_request_id = Some(id.clone());
                }
            }
        }
    }

    /// Convert accumulated state to a `ModelResponse`.
    ///
    /// # Errors
    ///
    /// Returns `ReasonerError` if the accumulated state is invalid.
    pub fn into_response(
        mut self,
        input_tokens: u64,
        latency_ms: u64,
    ) -> Result<ModelResponse, ReasonerError> {
        let effective_input_tokens = if self.input_tokens > 0 {
            self.input_tokens
        } else {
            input_tokens
        };

        // Recover any in-progress tool_use that was not finalized by a
        // ContentBlockStop (e.g. the stream was truncated). Without this
        // the tool is silently lost and the response looks like EndTurn
        // with no tool calls.
        if let Some(pending) = self.current_tool_use.take() {
            tracing::warn!(
                tool_name = %pending.name,
                tool_id = %pending.id,
                json_len = pending.input_json.len(),
                "Stream ended with an in-progress tool_use block — \
                 recovering partial tool call"
            );
            self.tool_uses.push(pending);
            if self.stop_reason.is_none() {
                self.stop_reason = Some(StopReason::MaxTokens);
            }
        }

        if let Some(err_msg) = self.stream_error.take() {
            // Always propagate a mid-stream error, even if partial text
            // or tool_use blocks arrived first. The previous behaviour
            // swallowed the error when any content was accumulated,
            // which caused partial tool-use blocks to be executed as if
            // the stream had finished cleanly -- a correctness bug that
            // could trigger malformed tool calls on the next iteration.
            //
            // NOTE: the caller (`complete_with_streaming`) only relies
            // on the `StreamAbortedWithPartial` variant for the narrow
            // case where a `tool_use` was in flight when the stream
            // died, so it can drive a per-tool-call retry that
            // re-issues a fresh streaming request and preserves the
            // partial input_json (preventing a dropped Write/Edit).
            // When there is NO in-flight tool, that retry loop adds
            // nothing — it just re-fires the identical request with
            // zero tracing output. We instead surface a generic
            // `Transient` so the existing
            // `stream_error_is_retryable` classifier routes the call
            // through `complete_and_emit_as_deltas`, which emits a
            // `StreamReset` to the UI and falls back to non-streaming
            // `provider.complete()` (whose retry loop has proper
            // tracing + body-cap shrink + provider fallback). Without
            // this branch a single Anthropic 5xx blip wedges chat in
            // an invisible 28-second silent-retry storm — see the
            // `fix-silent-stream-retry-storm` plan.
            //
            // Include model + message_id when known so the operator-visible
            // failure string is actionable (operators can correlate
            // `msg_id` with provider / router logs). The `err_msg`
            // string already carries the Anthropic `error.type` prefix
            // (see `anthropic::sse::parse_sse_event`) when the upstream
            // supplied one.
            let mut context_parts: Vec<String> = Vec::new();
            if !self.model.is_empty() {
                context_parts.push(format!("model={}", self.model));
            }
            if !self.message_id.is_empty() {
                context_parts.push(format!("msg_id={}", self.message_id));
            }
            if let Some(ref req_id) = self.provider_request_id {
                if !req_id.is_empty() {
                    context_parts.push(format!("request_id={req_id}"));
                }
            }
            let context = if context_parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", context_parts.join(", "))
            };
            let reason = format!("stream terminated with error{context}: {err_msg}");

            // The recovery path above (pre-stream-error) already pushed
            // a pending `current_tool_use` onto `self.tool_uses` and
            // set `current_tool_use` to None. For the aborted-stream
            // path we undo that: pop the recovered tool off
            // `self.tool_uses` if it is still there so the caller gets
            // it back as `partial_tool_use` instead of seeing it as a
            // completed tool call.
            let partial_tool_use = self.tool_uses.pop().map(|pending| PartialToolUse {
                tool_use_id: pending.id,
                tool_name: pending.name,
                partial_json: pending.input_json,
            });

            return match partial_tool_use {
                Some(partial) => Err(ReasonerError::StreamAbortedWithPartial {
                    reason,
                    partial_tool_use: Some(partial),
                }),
                None => Err(ReasonerError::Transient {
                    status: 502,
                    message: reason,
                    retry_after: None,
                }),
            };
        }

        let mut content_blocks = Vec::new();

        if !self.thinking_content.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: self.thinking_content,
                signature: self.thinking_signature,
            });
        }

        if !self.text_content.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: self.text_content,
            });
        }

        for tool in self.tool_uses {
            let input: serde_json::Value = if tool.input_json.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tool.input_json)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": tool.input_json }))
            };

            content_blocks.push(ContentBlock::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
            });
        }

        let message = Message {
            role: Role::Assistant,
            content: content_blocks,
        };

        let model_used = self.model.clone();

        Ok(ModelResponse {
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            message,
            usage: Usage {
                input_tokens: effective_input_tokens,
                output_tokens: self.output_tokens,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
            },
            trace: ProviderTrace {
                message_id: if self.message_id.is_empty() {
                    None
                } else {
                    Some(self.message_id)
                },
                provider_request_id: self.provider_request_id,
                latency_ms,
                model: self.model,
            },
            model_used,
        })
    }
}
