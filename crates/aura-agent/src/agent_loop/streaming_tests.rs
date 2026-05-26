use aura_reasoner::{
    ContentBlock, Message, MockProvider, ModelProvider, ModelRequest, ModelResponse, ProviderTrace,
    ReasonerError, StopReason, StreamContentType, StreamEvent, StreamEventStream, Usage,
};
use futures_util::stream;
use tokio::sync::mpsc;

use super::{AgentLoop, AgentLoopConfig};
use crate::events::AgentLoopEvent;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};

struct NoOpExecutor;

#[async_trait::async_trait]
impl AgentToolExecutor for NoOpExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        tool_calls
            .iter()
            .map(|tc| ToolCallResult::success(&tc.id, "ok"))
            .collect()
    }
}

struct StreamErrorProvider {
    fallback_text: String,
}

impl StreamErrorProvider {
    fn new(text: &str) -> Self {
        Self {
            fallback_text: text.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for StreamErrorProvider {
    fn name(&self) -> &'static str {
        "stream-error-test"
    }

    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        Ok(ModelResponse::new(
            StopReason::EndTurn,
            Message::assistant(&self.fallback_text),
            Usage::new(10, 5),
            ProviderTrace::new("test", 0),
        ))
    }

    async fn complete_streaming(
        &self,
        _request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let events = vec![
            Ok(StreamEvent::MessageStart {
                message_id: "msg_err".to_string(),
                model: "test".to_string(),
                input_tokens: Some(10),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_type: StreamContentType::Text,
            }),
            Ok(StreamEvent::TextDelta {
                text: "partial...".to_string(),
            }),
            Err(ReasonerError::Internal("Connection lost".to_string())),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

struct SuccessStreamProvider {
    inner: MockProvider,
}

#[async_trait::async_trait]
impl ModelProvider for SuccessStreamProvider {
    fn name(&self) -> &'static str {
        "success-stream-test"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let response = self.inner.complete(request).await?;
        let mut events: Vec<Result<StreamEvent, ReasonerError>> = Vec::new();

        events.push(Ok(StreamEvent::MessageStart {
            message_id: "msg_ok".to_string(),
            model: "mock-model".to_string(),
            input_tokens: Some(response.usage.input_tokens),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }));

        for (idx, block) in response.message.content.iter().enumerate() {
            let index = idx as u32;
            if let ContentBlock::Text { text } = block {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    content_type: StreamContentType::Text,
                }));
                events.push(Ok(StreamEvent::TextDelta { text: text.clone() }));
                events.push(Ok(StreamEvent::ContentBlockStop { index }));
            }
        }

        events.push(Ok(StreamEvent::MessageDelta {
            stop_reason: Some(response.stop_reason),
            output_tokens: response.usage.output_tokens,
        }));
        events.push(Ok(StreamEvent::MessageStop));

        Ok(Box::pin(stream::iter(events)))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

fn default_config() -> AgentLoopConfig {
    AgentLoopConfig {
        system_prompt: "streaming test agent".to_string(),
        // These tests exercise the legacy buffered-streaming path's
        // `retry_streaming_for_partial_tool_use` / `StreamReset` flow,
        // which has not yet been ported onto the pump's
        // `ResponseEventStream` (deferred per Phase E.4 plan note).
        // Pin them to the legacy path until the pump-side port lands.
        use_stream_pump: false,
        ..AgentLoopConfig::default()
    }
}

async fn collect_events(mut rx: mpsc::Receiver<AgentLoopEvent>) -> Vec<AgentLoopEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn stream_error_emits_reset_before_fallback() {
    let provider = StreamErrorProvider::new("Complete fallback response");
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    let result = agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);

    let events = collect_events(rx).await;
    let reset_pos = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::StreamReset { .. }));
    assert!(reset_pos.is_some(), "StreamReset event must be emitted");

    let has_text_after_reset = events[reset_pos.unwrap()..]
        .iter()
        .any(|e| matches!(e, AgentLoopEvent::TextDelta(_)));
    assert!(
        has_text_after_reset,
        "TextDelta must follow StreamReset with complete content"
    );
}

#[tokio::test]
async fn stream_reset_followed_by_complete_content() {
    let fallback_text = "The authoritative fallback text";
    let provider = StreamErrorProvider::new(fallback_text);
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .unwrap();

    let events = collect_events(rx).await;
    let reset_idx = events
        .iter()
        .position(|e| matches!(e, AgentLoopEvent::StreamReset { .. }))
        .expect("StreamReset must be present");

    let post_reset_text: String = events[reset_idx..]
        .iter()
        .filter_map(|e| match e {
            AgentLoopEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(post_reset_text, fallback_text);
}

#[tokio::test]
async fn successful_stream_no_reset() {
    let provider = SuccessStreamProvider {
        inner: MockProvider::simple_response("Success!"),
    };
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .unwrap();

    let events = collect_events(rx).await;
    let has_reset = events
        .iter()
        .any(|e| matches!(e, AgentLoopEvent::StreamReset { .. }));
    assert!(
        !has_reset,
        "No StreamReset should be emitted on a successful stream"
    );
}

#[tokio::test]
async fn stream_error_emits_exactly_one_reset() {
    let provider = StreamErrorProvider::new("Fallback");
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .unwrap();

    let events = collect_events(rx).await;
    let reset_count = events
        .iter()
        .filter(|e| matches!(e, AgentLoopEvent::StreamReset { .. }))
        .count();
    assert_eq!(
        reset_count, 1,
        "Exactly one StreamReset should be emitted per fallback"
    );
}

// ---------------------------------------------------------------------------
// Per-tool-call streaming retry (StreamAbortedWithPartial)
// ---------------------------------------------------------------------------

/// Mock provider whose `complete_streaming` emits a `tool_use`
/// `content_block_start` + a partial `input_json_delta`, then a
/// mid-stream SSE `Error` event before `content_block_stop`. The
/// `StreamAccumulator` turns this into
/// `ReasonerError::StreamAbortedWithPartial` inside the agent's
/// streaming call -- exactly the retry trigger we want to test.
///
/// The `fail_count` counter decides how many attempts to fail before
/// finally emitting a clean `MessageStop`; `usize::MAX` means "always
/// fail" (retry-budget-exhaustion test).
struct FlakyPartialProvider {
    fail_count: std::sync::atomic::AtomicUsize,
    success_text: String,
}

impl FlakyPartialProvider {
    fn new(fail_count: usize, text: &str) -> Self {
        Self {
            fail_count: std::sync::atomic::AtomicUsize::new(fail_count),
            success_text: text.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for FlakyPartialProvider {
    fn name(&self) -> &'static str {
        "flaky-partial-test"
    }

    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        Ok(ModelResponse::new(
            StopReason::EndTurn,
            Message::assistant(&self.success_text),
            Usage::new(1, 1),
            ProviderTrace::new("test", 0),
        ))
    }

    async fn complete_streaming(
        &self,
        _request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let remaining = self
            .fail_count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if remaining == 0 {
            // Hold the counter at 0 so subsequent retries (if any) still hit
            // the success path rather than underflowing into usize::MAX.
            self.fail_count
                .store(0, std::sync::atomic::Ordering::SeqCst);
            // Success path: one text-only response.
            let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
                Ok(StreamEvent::MessageStart {
                    message_id: "msg_ok".to_string(),
                    model: "test".to_string(),
                    input_tokens: Some(1),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    content_type: StreamContentType::Text,
                }),
                Ok(StreamEvent::TextDelta {
                    text: self.success_text.clone(),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::EndTurn),
                    output_tokens: 1,
                }),
                Ok(StreamEvent::MessageStop),
            ];
            return Ok(Box::pin(stream::iter(events)));
        }
        // Failure path: start a tool_use, drop a partial input_json_delta,
        // then emit an SSE Error event before the stream ends. No
        // content_block_stop -> accumulator will see an in-flight tool.
        let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
            Ok(StreamEvent::MessageStart {
                message_id: "msg_fail".to_string(),
                model: "test".to_string(),
                input_tokens: Some(1),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_type: StreamContentType::ToolUse {
                    id: "toolu_partial".to_string(),
                    name: "write_file".to_string(),
                },
            }),
            Ok(StreamEvent::InputJsonDelta {
                partial_json: "{\"path\":\"src/".to_string(),
            }),
            Ok(StreamEvent::Error {
                message: "overloaded_error: upstream flaked".to_string(),
                request_id: None,
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

/// Shared env-var guard so the retry-budget tests can pin
/// `AURA_LLM_MAX_RETRIES` / backoff settings without racing each
/// other or the config tests in aura-reasoner.
static STREAM_RETRY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // intentional: serializes env var edits across async awaits
async fn stream_aborted_with_partial_retries_then_succeeds() {
    // Provider fails twice with StreamAbortedWithPartial, then
    // succeeds. The retry loop must emit two ToolCallRetrying events
    // (one before each retry sleep) and finally return the success
    // response. The backoff envs are pinned tiny so the test is fast.
    let _lock = STREAM_RETRY_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g1 = EnvGuard::set("AURA_LLM_MAX_RETRIES", "5");
    let _g2 = EnvGuard::set("AURA_LLM_BACKOFF_INITIAL_MS", "1");
    let _g3 = EnvGuard::set("AURA_LLM_BACKOFF_CAP_MS", "2");

    let provider = FlakyPartialProvider::new(2, "recovered");
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    let result = agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await
        .expect("retry should eventually succeed");
    assert_eq!(result.iterations, 1);

    let events = collect_events(rx).await;
    let retrying = events
        .iter()
        .filter(|e| matches!(e, AgentLoopEvent::ToolCallRetrying { .. }))
        .count();
    assert_eq!(
        retrying, 2,
        "expected exactly two ToolCallRetrying events, got: {retrying}"
    );

    let failed = events
        .iter()
        .any(|e| matches!(e, AgentLoopEvent::ToolCallFailed { .. }));
    assert!(!failed, "success path must not emit ToolCallFailed");

    // Sanity-check the partial tool identity is preserved across
    // retries: both retry events should name the write_file tool.
    let any_write_file_retry = events.iter().any(|e| match e {
        AgentLoopEvent::ToolCallRetrying { tool_name, .. } => tool_name == "write_file",
        _ => false,
    });
    assert!(
        any_write_file_retry,
        "retry events should carry the original tool_name (write_file)"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // intentional: serializes env var edits across async awaits
async fn stream_aborted_with_partial_exhausts_and_fails() {
    // Provider always fails. With max_retries=2 the loop must emit 2
    // retries then a final ToolCallFailed and surface the error.
    let _lock = STREAM_RETRY_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g1 = EnvGuard::set("AURA_LLM_MAX_RETRIES", "2");
    let _g2 = EnvGuard::set("AURA_LLM_BACKOFF_INITIAL_MS", "1");
    let _g3 = EnvGuard::set("AURA_LLM_BACKOFF_CAP_MS", "2");

    let provider = FlakyPartialProvider::new(1_000, "never-used");
    let executor = NoOpExecutor;
    let agent = AgentLoop::new(default_config());
    let (tx, rx) = mpsc::channel(1024);
    let messages = vec![Message::user("hello")];

    let result = agent
        .run_with_events(&provider, &executor, messages, vec![], Some(tx), None)
        .await;
    // The agent loop surfaces the final error through AgentLoopResult
    // rather than the outer Result (the model error is recorded, not
    // returned). Either shape is acceptable -- we just need
    // ToolCallFailed to have been emitted.
    let _ = result; // silence unused warning on the Ok path

    let events = collect_events(rx).await;
    let retrying = events
        .iter()
        .filter(|e| matches!(e, AgentLoopEvent::ToolCallRetrying { .. }))
        .count();
    assert_eq!(
        retrying, 2,
        "expected two retries before exhaustion, got: {retrying}"
    );

    let failed = events
        .iter()
        .filter(|e| matches!(e, AgentLoopEvent::ToolCallFailed { .. }))
        .count();
    assert_eq!(
        failed, 1,
        "expected exactly one ToolCallFailed after exhaustion, got: {failed}"
    );
}
