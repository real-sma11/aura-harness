//! Kernel-recorded streaming adapter.
//!
//! [`RecordingStream`] wraps a `StreamEventStream` together with an owned
//! [`ReasonStreamHandle`] and guarantees that Invariant §3 ("every LLM
//! call is recorded") holds even for streaming completions:
//!
//! - on natural end of stream (`MessageStop` or the underlying stream
//!   yielding `None`) the handle is consumed by
//!   [`ReasonStreamHandle::record_completed`];
//! - on any `StreamEvent::Error` or transport `Err(_)` item the handle is
//!   consumed by [`ReasonStreamHandle::record_failed`];
//! - if the `RecordingStream` is dropped before either of the above
//!   fires, the `Drop` impl finalizes the handle with
//!   `record_failed("stream dropped before completion")`.
//!
//! The underlying handle is consumed by value on every finalization path,
//! so double-recording is impossible by construction.

use std::pin::Pin;
use std::task::{Context, Poll};

use aura_agent_kernel::ReasonStreamHandle;
use aura_model_reasoner::{ReasonerError, StreamAccumulator, StreamEvent, StreamEventStream};
use futures_util::Stream;

/// Reason string used when `RecordingStream` is dropped before any
/// terminal event (success, error, or end-of-stream) fires. Tests rely
/// on the exact value to detect mid-stream cancellation.
pub(crate) const STREAM_DROPPED_REASON: &str = "stream dropped before completion";

/// A stream wrapper that records the outcome of a streaming model call
/// through a [`ReasonStreamHandle`] exactly once.
pub(crate) struct RecordingStream {
    inner: StreamEventStream,
    handle: Option<ReasonStreamHandle>,
    accumulator: StreamAccumulator,
}

impl RecordingStream {
    pub(crate) fn new(inner: StreamEventStream, handle: ReasonStreamHandle) -> Self {
        Self {
            inner,
            handle: Some(handle),
            accumulator: StreamAccumulator::new(),
        }
    }

    fn finalize_completed(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let tool_uses: Vec<String> = self
            .accumulator
            .tool_uses
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let model = if self.accumulator.model.is_empty() {
            "unknown".to_string()
        } else {
            self.accumulator.model.clone()
        };
        let stop_reason = self
            .accumulator
            .stop_reason
            .unwrap_or(aura_model_reasoner::StopReason::EndTurn);
        let usage = aura_model_reasoner::Usage {
            input_tokens: self.accumulator.input_tokens,
            output_tokens: self.accumulator.output_tokens,
            cache_creation_input_tokens: self.accumulator.cache_creation_input_tokens,
            cache_read_input_tokens: self.accumulator.cache_read_input_tokens,
        };

        if let Err(e) = handle.record_completed(&model, stop_reason, &usage, &tool_uses) {
            tracing::warn!(error = %e, "RecordingStream failed to record completed reasoning entry");
        }
    }

    fn finalize_failed(&mut self, reason: &str) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Err(e) = handle.record_failed(reason) {
            tracing::warn!(
                error = %e,
                reason,
                "RecordingStream failed to record failed reasoning entry"
            );
        }
    }
}

impl Stream for RecordingStream {
    type Item = Result<StreamEvent, ReasonerError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: structural pinning for `inner` only; all other fields
        // are either `Unpin` (`Option<ReasonStreamHandle>`,
        // `StreamAccumulator`) or accessed without moving.
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                this.accumulator.process(&event);
                match &event {
                    StreamEvent::MessageStop => {
                        this.finalize_completed();
                    }
                    StreamEvent::Error { message, .. } => {
                        let reason = message.clone();
                        this.finalize_failed(&reason);
                    }
                    _ => {}
                }
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(Some(Err(err))) => {
                let reason = err.to_string();
                this.finalize_failed(&reason);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                // Natural end without an explicit `MessageStop` — still
                // finalize so Invariant §3 holds.
                if this.handle.is_some() {
                    this.finalize_completed();
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for RecordingStream {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.finalize_failed(STREAM_DROPPED_REASON);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
    use aura_core_types::{AgentId, TransactionType};
    use aura_model_reasoner::{Message, MockProvider, ModelProvider, ModelRequest, StreamEvent};
    use aura_store_db::{RocksStore, Store};
    use futures_util::{stream, StreamExt};
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TwoEventProvider;

    #[async_trait::async_trait]
    impl ModelProvider for TwoEventProvider {
        fn name(&self) -> &'static str {
            "two-event-mock"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
        ) -> Result<aura_model_reasoner::ModelResponse, ReasonerError> {
            Err(ReasonerError::Internal("streaming-only mock".into()))
        }

        async fn complete_streaming(
            &self,
            _request: ModelRequest,
        ) -> Result<StreamEventStream, ReasonerError> {
            let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
                Ok(StreamEvent::MessageStart {
                    message_id: "msg-1".to_string(),
                    model: "mock-stream-model".to_string(),
                    input_tokens: Some(3),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
                Ok(StreamEvent::TextDelta {
                    text: "partial".to_string(),
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn create_kernel(provider: Arc<dyn ModelProvider + Send + Sync>) -> (Arc<Kernel>, TempDir) {
        let db_dir = TempDir::new().unwrap();
        let ws_dir = TempDir::new().unwrap();
        let agent_id = AgentId::generate();
        let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
        let executor = ExecutorRouter::new();
        let config = KernelConfig {
            workspace_base: ws_dir.path().to_path_buf(),
            ..KernelConfig::default()
        };
        let kernel = Arc::new(Kernel::new(store, provider, executor, config, agent_id).unwrap());
        (kernel, db_dir)
    }

    // Provider that yields two events, then a transport-level
    // `Err(ReasonerError)`. Exercises the mid-stream error finalization
    // path (Invariant §3 / T3.a).
    struct ErrorMidStreamProvider;

    #[async_trait::async_trait]
    impl ModelProvider for ErrorMidStreamProvider {
        fn name(&self) -> &'static str {
            "error-midstream-mock"
        }

        async fn complete(
            &self,
            _request: ModelRequest,
        ) -> Result<aura_model_reasoner::ModelResponse, ReasonerError> {
            Err(ReasonerError::Internal("streaming-only mock".into()))
        }

        async fn complete_streaming(
            &self,
            _request: ModelRequest,
        ) -> Result<aura_model_reasoner::StreamEventStream, ReasonerError> {
            let events: Vec<Result<StreamEvent, ReasonerError>> = vec![
                Ok(StreamEvent::MessageStart {
                    message_id: "msg-err-1".to_string(),
                    model: "mock-stream-model".to_string(),
                    input_tokens: Some(5),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
                Ok(StreamEvent::TextDelta {
                    text: "partial".to_string(),
                }),
                Err(ReasonerError::Internal(
                    "simulated mid-stream failure".into(),
                )),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    // ----------------------------------------------------------------
    // Invariant §3 enforcement — streaming recording tests.
    //
    // The three tests below pin Invariant §3 ("every LLM call is
    // recorded") against each terminal path of `RecordingStream`:
    //
    //   - `streaming_natural_end_records_completed`: `MessageStop` /
    //     end-of-stream → exactly one Reasoning entry, not failed.
    //   - `streaming_error_records_failed`: a mid-stream
    //     `Err(ReasonerError)` → exactly one failed Reasoning entry,
    //     no second entry from the `Drop` path.
    //   - `streaming_drop_records_failed`: early `drop()` before any
    //     terminal event → exactly one failed entry with
    //     `STREAM_DROPPED_REASON`.
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn streaming_drop_records_failed() {
        let provider: Arc<dyn ModelProvider + Send + Sync> = Arc::new(TwoEventProvider);
        let (kernel, _db) = create_kernel(provider);

        let request = ModelRequest::builder("mock-stream-model", "system")
            .message(Message::user("hi"))
            .try_build()
            .unwrap();
        let (handle, inner) = kernel.reason_streaming(request).await.unwrap();
        let mut stream = RecordingStream::new(inner, handle);

        // Consume only the first event, then drop the stream.
        let first = stream.next().await;
        assert!(first.is_some(), "expected at least one streamed event");
        drop(stream);

        // The kernel's store should now contain exactly one Reasoning
        // entry marked as failed with the mid-stream drop reason.
        let entries = kernel
            .store()
            .scan_record(kernel.agent_id, 0, 64)
            .expect("scan_record");
        let reasoning: Vec<_> = entries
            .iter()
            .filter(|e| e.tx.tx_type == TransactionType::Reasoning)
            .collect();
        assert_eq!(
            reasoning.len(),
            1,
            "expected exactly one reasoning entry, got {}",
            reasoning.len()
        );

        let payload: serde_json::Value =
            serde_json::from_slice(&reasoning[0].tx.payload).expect("reasoning payload is json");
        assert_eq!(
            payload.get("status").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert_eq!(
            payload.get("error").and_then(|v| v.as_str()),
            Some(STREAM_DROPPED_REASON)
        );
    }

    #[tokio::test]
    async fn streaming_natural_end_records_completed() {
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("hello world"));
        let (kernel, _db) = create_kernel(provider);

        let request = ModelRequest::builder("test-model", "system")
            .message(Message::user("hi"))
            .try_build()
            .unwrap();
        let (handle, inner) = kernel.reason_streaming(request).await.unwrap();
        let mut stream = RecordingStream::new(inner, handle);
        while stream.next().await.is_some() {}
        drop(stream);

        let entries = kernel
            .store()
            .scan_record(kernel.agent_id, 0, 64)
            .expect("scan_record");
        let reasoning: Vec<_> = entries
            .iter()
            .filter(|e| e.tx.tx_type == TransactionType::Reasoning)
            .collect();
        assert_eq!(reasoning.len(), 1);

        let payload: serde_json::Value =
            serde_json::from_slice(&reasoning[0].tx.payload).expect("reasoning payload is json");
        assert!(
            payload.get("status").is_none(),
            "natural end must not tag the entry as failed: {payload}"
        );
        assert!(payload.get("stop_reason").is_some());
    }

    #[tokio::test]
    async fn streaming_error_records_failed() {
        let provider: Arc<dyn ModelProvider + Send + Sync> = Arc::new(ErrorMidStreamProvider);
        let (kernel, _db) = create_kernel(provider);

        let request = ModelRequest::builder("mock-stream-model", "system")
            .message(Message::user("hi"))
            .try_build()
            .unwrap();
        let (handle, inner) = kernel.reason_streaming(request).await.unwrap();
        let mut stream = RecordingStream::new(inner, handle);

        // Drain the stream fully. The third item is `Err(_)` which
        // must trigger the failed-finalization path. After drain the
        // stream is exhausted and `drop()` must observe `handle ==
        // None` (otherwise it would append a second failure entry —
        // see the double-finalization assertion below).
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                saw_error = true;
            }
        }
        assert!(saw_error, "mock stream must yield exactly one error item");
        drop(stream);

        let entries = kernel
            .store()
            .scan_record(kernel.agent_id, 0, 64)
            .expect("scan_record");
        let reasoning: Vec<_> = entries
            .iter()
            .filter(|e| e.tx.tx_type == TransactionType::Reasoning)
            .collect();

        // Exactly one reasoning entry — the `Drop` finalizer must be a
        // no-op after an explicit error finalization.
        assert_eq!(
            reasoning.len(),
            1,
            "expected exactly one reasoning entry after mid-stream error, got {}: {:?}",
            reasoning.len(),
            reasoning.iter().map(|e| &e.tx.payload).collect::<Vec<_>>(),
        );

        let payload: serde_json::Value =
            serde_json::from_slice(&reasoning[0].tx.payload).expect("reasoning payload is json");
        assert_eq!(
            payload.get("status").and_then(|v| v.as_str()),
            Some("failed"),
            "expected status=failed, got: {payload}"
        );
        let err_text = payload
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error field present");
        assert!(
            err_text.contains("simulated mid-stream failure"),
            "expected mid-stream error text, got: {err_text}"
        );
        assert_ne!(
            err_text, STREAM_DROPPED_REASON,
            "mid-stream error must not be attributed to the Drop path"
        );
    }
}
