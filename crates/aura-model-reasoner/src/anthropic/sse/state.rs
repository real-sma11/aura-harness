//! Buffered SSE state machine — [`SseStream`].
//!
//! Wraps an arbitrary byte stream
//! (`Stream<Item = Result<bytes::Bytes, _>>`) and emits parsed
//! [`StreamEvent`]s. On first poll, surfaces a synthetic
//! [`StreamEvent::HttpMeta`] carrying the captured `x-request-id` so
//! the [`crate::StreamAccumulator`] can seed `provider_request_id`
//! deterministically before any provider event arrives.
//!
//! The state machine is deliberately simple — three terminal
//! conditions:
//!
//! 1. The upstream stream closes (`Poll::Ready(None)`).
//! 2. A [`StreamEvent::MessageStop`] / [`StreamEvent::Error`] is
//!    parsed (`finished = true` short-circuits subsequent polls).
//! 3. The buffer exceeds [`MAX_SSE_BUFFER_SIZE`] (returned as a
//!    [`ReasonerError::Internal`] to guard against runaway upstreams).
//!
//! [`MAX_SSE_BUFFER_SIZE`]: super::parse::MAX_SSE_BUFFER_SIZE

use super::parse::{parse_sse_event, pop_event_block, MAX_SSE_BUFFER_SIZE};
use crate::error::ReasonerError;
use crate::StreamEvent;
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A stream that parses SSE events from an HTTP byte stream.
///
/// On first poll, the stream yields a synthetic
/// [`StreamEvent::HttpMeta`] carrying the HTTP `x-request-id` (if the
/// caller captured one before consuming the response body). Subsequent
/// polls emit the provider's SSE events as usual. This is the only
/// path that surfaces the request id from the *streaming* HTTP
/// response — the provider's wire protocol never includes it inside
/// the SSE body, and the response headers are gone once
/// `response.bytes_stream()` has been called, so the capture has to
/// happen at the HTTP layer and then flow through this type.
pub(in crate::anthropic) struct SseStream<S> {
    inner: S,
    buffer: String,
    finished: bool,
    /// `x-request-id` from the HTTP response headers, set by
    /// [`SseStream::with_request_id`]. `None` means the caller didn't
    /// capture one (or the upstream didn't send one).
    request_id: Option<String>,
    /// Whether the synthetic `HttpMeta` event has already been
    /// emitted. Ensures we only surface it once, before any provider
    /// event.
    emitted_http_meta: bool,
}

impl<S> SseStream<S> {
    /// Retained for test-only construction of a stream without header
    /// capture. Production call sites go through
    /// [`Self::with_request_id`] so the synthetic `HttpMeta` preamble
    /// actually carries the HTTP `x-request-id`.
    #[cfg(test)]
    pub(in crate::anthropic) const fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            finished: false,
            request_id: None,
            emitted_http_meta: false,
        }
    }

    /// Construct a stream that will emit a synthetic
    /// [`StreamEvent::HttpMeta`] carrying `request_id` before the
    /// first provider event.
    pub(in crate::anthropic) const fn with_request_id(
        inner: S,
        request_id: Option<String>,
    ) -> Self {
        Self {
            inner,
            buffer: String::new(),
            finished: false,
            request_id,
            emitted_http_meta: false,
        }
    }
}

impl<S, E> Stream for SseStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    type Item = Result<StreamEvent, ReasonerError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }

        // Emit the synthetic HttpMeta event once, before any provider
        // event. Doing this before `parse_next_event` guarantees a
        // consumer that inspects the first event (e.g. to seed a
        // `StreamAccumulator`) never has to race against the first
        // `message_start`.
        if !self.emitted_http_meta {
            self.emitted_http_meta = true;
            let request_id = self.request_id.clone();
            return Poll::Ready(Some(Ok(StreamEvent::HttpMeta { request_id })));
        }

        loop {
            if let Some(event) = self.parse_next_event() {
                if matches!(event, StreamEvent::MessageStop | StreamEvent::Error { .. }) {
                    self.finished = true;
                }
                return Poll::Ready(Some(Ok(event)));
            }

            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    match std::str::from_utf8(&bytes) {
                        Ok(s) => self.buffer.push_str(s),
                        Err(e) => {
                            return Poll::Ready(Some(Ok(StreamEvent::Error {
                                message: format!("invalid UTF-8 in SSE stream: {e}"),
                                request_id: None,
                            })));
                        }
                    }
                    if self.buffer.len() > MAX_SSE_BUFFER_SIZE {
                        self.finished = true;
                        return Poll::Ready(Some(Err(ReasonerError::Internal(format!(
                            "SSE buffer exceeded {MAX_SSE_BUFFER_SIZE} bytes"
                        )))));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.finished = true;
                    return Poll::Ready(Some(Err(ReasonerError::Request(format!(
                        "Stream error: {e}"
                    )))));
                }
                Poll::Ready(None) => {
                    self.finished = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S> SseStream<S> {
    /// Pop one complete event block from the internal buffer and
    /// translate it into a [`StreamEvent`]. Returns `None` when the
    /// buffer doesn't yet hold a complete block.
    fn parse_next_event(&mut self) -> Option<StreamEvent> {
        let event_str = pop_event_block(&mut self.buffer)?;
        parse_sse_event(&event_str)
    }
}
