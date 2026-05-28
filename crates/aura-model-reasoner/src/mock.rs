//! Mock implementations for testing.
//!
//! Provides `MockProvider` which implements `ModelProvider` for testing
//! the turn processor and other components without calling a real model API.

use crate::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, ProviderTrace, Role,
    StopReason, Usage,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tracing::debug;

// ============================================================================
// MockProvider (Spec-02)
// ============================================================================

/// Mock model provider for testing.
///
/// Allows configuring predefined responses for testing the turn processor
/// and other components without calling a real model API.
pub struct MockProvider {
    /// Responses to return in sequence
    responses: Mutex<Vec<MockResponse>>,
    /// Call counter
    call_count: AtomicU64,
    /// Default response if no configured responses remain
    default_response: MockResponse,
    /// Simulated latency in milliseconds
    latency_ms: u64,
    /// Whether to fail
    should_fail: bool,
}

/// A mock response configuration.
#[derive(Debug, Clone)]
pub struct MockResponse {
    /// Stop reason
    pub stop_reason: StopReason,
    /// Content blocks to return
    pub content: Vec<ContentBlock>,
    /// Usage to report
    pub usage: Usage,
}

impl Default for MockResponse {
    fn default() -> Self {
        Self {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text("Mock response")],
            usage: Usage::new(100, 50),
        }
    }
}

impl MockResponse {
    /// Create a simple text response.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text(text)],
            usage: Usage::new(100, 50),
        }
    }

    /// Create a response with tool use.
    #[must_use]
    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            stop_reason: StopReason::ToolUse,
            content: vec![ContentBlock::tool_use(id, name, input)],
            usage: Usage::new(100, 50),
        }
    }

    /// Override the stop reason on an existing response.
    #[must_use]
    pub const fn with_stop_reason(mut self, stop_reason: StopReason) -> Self {
        self.stop_reason = stop_reason;
        self
    }

    /// Create a response with text and tool use.
    #[must_use]
    pub fn text_and_tool(
        text: impl Into<String>,
        tool_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
    ) -> Self {
        Self {
            stop_reason: StopReason::ToolUse,
            content: vec![
                ContentBlock::text(text),
                ContentBlock::tool_use(tool_id, tool_name, tool_input),
            ],
            usage: Usage::new(100, 50),
        }
    }
}

impl MockProvider {
    /// Create a new mock provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(Vec::new()),
            call_count: AtomicU64::new(0),
            default_response: MockResponse::default(),
            latency_ms: 0,
            should_fail: false,
        }
    }

    /// Add a response to return on the next call.
    #[must_use]
    pub fn with_response(self, response: MockResponse) -> Self {
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(response);
        self
    }

    /// Add multiple responses to return in sequence.
    #[must_use]
    pub fn with_responses(self, responses: Vec<MockResponse>) -> Self {
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(responses);
        self
    }

    /// Set the default response when no configured responses remain.
    #[must_use]
    pub fn with_default_response(mut self, response: MockResponse) -> Self {
        self.default_response = response;
        self
    }

    /// Set simulated latency.
    #[must_use]
    pub const fn with_latency(mut self, latency_ms: u64) -> Self {
        self.latency_ms = latency_ms;
        self
    }

    /// Configure to fail.
    #[must_use]
    pub const fn with_failure(mut self) -> Self {
        self.should_fail = true;
        self
    }

    /// Get the number of times `complete` was called.
    #[must_use]
    pub fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Create a mock that returns a file read tool use.
    #[must_use]
    pub fn file_reader(path: impl Into<String>) -> Self {
        Self::new().with_response(MockResponse::tool_use(
            "tool_1",
            "fs.read",
            serde_json::json!({ "path": path.into() }),
        ))
    }

    /// Create a mock that returns a simple text response.
    #[must_use]
    pub fn simple_response(text: impl Into<String>) -> Self {
        Self::new().with_response(MockResponse::text(text))
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, crate::ReasonerError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        debug!(
            call_count = self.call_count(),
            model = %request.model,
            messages = request.messages.len(),
            "MockProvider.complete called"
        );

        if self.should_fail {
            return Err(crate::ReasonerError::Internal(
                "Mock provider configured to fail".into(),
            ));
        }

        // Simulate latency
        if self.latency_ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(self.latency_ms)).await;
        }

        let response = {
            let mut responses = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if responses.is_empty() {
                self.default_response.clone()
            } else {
                responses.remove(0)
            }
        };

        Ok(ModelResponse {
            stop_reason: response.stop_reason,
            message: Message {
                role: Role::Assistant,
                content: response.content,
            },
            usage: response.usage,
            trace: ProviderTrace::new("mock-model", self.latency_ms),
            model_used: "mock-model".to_string(),
        })
    }

    async fn health_check(&self) -> bool {
        !self.should_fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider_simple() {
        let provider = MockProvider::simple_response("Hello from mock!");

        let request = ModelRequest::builder("test-model", "Test system")
            .message(Message::user("Hi"))
            .try_build()
            .unwrap();

        let response = provider.complete(request).await.unwrap();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.message.text_content(), "Hello from mock!");
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn test_mock_provider_tool_use() {
        let provider = MockProvider::file_reader("test.txt");

        let request = ModelRequest::builder("test-model", "Test system")
            .message(Message::user("Read a file"))
            .try_build()
            .unwrap();

        let response = provider.complete(request).await.unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(response.message.has_tool_use());
    }

    #[tokio::test]
    async fn test_mock_provider_sequence() {
        let provider = MockProvider::new()
            .with_response(MockResponse::tool_use(
                "1",
                "fs.ls",
                serde_json::json!({"path": "."}),
            ))
            .with_response(MockResponse::text("Done!"));

        let request = ModelRequest::builder("test-model", "System")
            .message(Message::user("List files"))
            .try_build()
            .unwrap();

        // First call returns tool use
        let r1 = provider.complete(request.clone()).await.unwrap();
        assert_eq!(r1.stop_reason, StopReason::ToolUse);

        // Second call returns text
        let r2 = provider.complete(request).await.unwrap();
        assert_eq!(r2.stop_reason, StopReason::EndTurn);
        assert_eq!(r2.message.text_content(), "Done!");
    }

    #[tokio::test]
    async fn test_mock_provider_failure() {
        let provider = MockProvider::new().with_failure();

        let request = ModelRequest::builder("test-model", "System")
            .message(Message::user("Hi"))
            .try_build()
            .unwrap();

        let result = provider.complete(request).await;
        assert!(result.is_err());
    }
}
