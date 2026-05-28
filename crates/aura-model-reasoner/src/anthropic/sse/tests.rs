//! Unit tests for the SSE submodules.
//!
//! Covers:
//!
//! - [`super::parse::parse_sse_event`] edge cases (ping, malformed
//!   JSON, missing data line, Anthropic-shape `error.type`
//!   passthrough, content-block deltas, message-delta stop reasons).
//! - [`super::state::SseStream`] state-machine behaviour: synthetic
//!   `HttpMeta` preamble, partial-chunk reassembly, CRLF tolerance,
//!   error-as-terminal, multi-event blocks.

use super::parse::parse_sse_event;
use super::state::SseStream;
use crate::{StopReason, StreamEvent};
use futures_util::{Stream, StreamExt};

fn bytes_stream(
    chunks: Vec<&'static str>,
) -> impl Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin {
    futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from(c.to_string()))),
    )
}

// --- parse_sse_event unit tests ---

#[test]
fn test_parse_ping_event() {
    let event = parse_sse_event("event: ping\ndata: {}");
    assert!(matches!(event, Some(StreamEvent::Ping)));
}

#[test]
fn test_parse_event_without_data_returns_none() {
    let event = parse_sse_event("event: message_start");
    assert!(event.is_none());
}

#[test]
fn test_parse_event_with_invalid_json_returns_error() {
    let event = parse_sse_event("event: message_start\ndata: {not valid json!!}");
    assert!(
        matches!(event, Some(StreamEvent::Error { ref message, .. }) if message.contains("malformed SSE JSON")),
        "expected StreamEvent::Error, got {event:?}"
    );
}

#[test]
fn test_parse_message_stop_event() {
    let event = parse_sse_event("event: message_stop\ndata: {\"type\":\"message_stop\"}");
    assert!(matches!(event, Some(StreamEvent::MessageStop)));
}

#[test]
fn test_parse_error_event() {
    let event = parse_sse_event(
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}",
    );
    match event {
        Some(StreamEvent::Error {
            message,
            request_id,
        }) => {
            assert_eq!(message, "overloaded");
            assert_eq!(request_id, None);
        }
        other => panic!("Expected Error event, got {other:?}"),
    }
}

#[test]
fn test_parse_sseerror_with_request_id_field_in_body() {
    // Some proxies (notably `aura-router`) embed the originating
    // request id inside the SSE error body. Forward it on
    // `StreamEvent::Error.request_id` so the accumulator can adopt
    // it when the response-header `x-request-id` was unavailable.
    let event = parse_sse_event(
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Internal server error\",\"request_id\":\"req_01XYZ\"}}",
    );
    match event {
        Some(StreamEvent::Error {
            message,
            request_id,
        }) => {
            assert_eq!(message, "api_error: Internal server error");
            assert_eq!(request_id.as_deref(), Some("req_01XYZ"));
        }
        other => panic!("Expected Error event, got {other:?}"),
    }
}

#[test]
fn test_parse_error_event_preserves_anthropic_error_type() {
    // Anthropic SSE wire format: `error.type` distinguishes
    // `overloaded_error` (retryable per-provider) from
    // `api_error` (generic 5xx). The reasoner's downstream retry
    // policy keys off this string, so the parser must forward it.
    let event = parse_sse_event(
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"service is overloaded\"}}",
    );
    match event {
        Some(StreamEvent::Error { message, .. }) => {
            assert_eq!(message, "overloaded_error: service is overloaded");
        }
        other => panic!("Expected Error event, got {other:?}"),
    }
}

#[test]
fn test_parse_error_event_without_type_uses_raw_message() {
    // Proxies sometimes emit a bare `{"error":{"message":"..."}}`
    // with no `type` — preserve the raw message verbatim so the
    // downstream classifier still sees the original prose.
    let event = parse_sse_event(
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Internal server error\"}}",
    );
    match event {
        Some(StreamEvent::Error { message, .. }) => {
            assert_eq!(message, "Internal server error");
        }
        other => panic!("Expected Error event, got {other:?}"),
    }
}

#[test]
fn test_parse_content_block_delta_text() {
    let event = parse_sse_event(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
    );
    match event {
        Some(StreamEvent::TextDelta { text }) => assert_eq!(text, "hi"),
        other => panic!("Expected TextDelta, got {other:?}"),
    }
}

#[test]
fn test_parse_message_delta_stop_reasons() {
    for (reason_str, expected) in [
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxTokens),
        ("stop_sequence", StopReason::StopSequence),
        ("end_turn", StopReason::EndTurn),
        ("unknown_reason", StopReason::EndTurn),
    ] {
        let data = format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason_str}\"}},\"usage\":{{\"output_tokens\":42}}}}"
        );
        let event = parse_sse_event(&data);
        match event {
            Some(StreamEvent::MessageDelta {
                stop_reason,
                output_tokens,
            }) => {
                assert_eq!(stop_reason, Some(expected));
                assert_eq!(output_tokens, 42);
            }
            other => panic!("Expected MessageDelta, got {other:?}"),
        }
    }
}

#[test]
fn test_parse_message_delta_no_usage() {
    let event = parse_sse_event(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null},\"usage\":null}",
    );
    match event {
        Some(StreamEvent::MessageDelta { output_tokens, .. }) => {
            assert_eq!(output_tokens, 0);
        }
        other => panic!("Expected MessageDelta, got {other:?}"),
    }
}

#[test]
fn test_parse_deepseek_usage_aliases() {
    let event = parse_sse_event(
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"deepseek-v4-flash","usage":{"prompt_tokens":100,"prompt_cache_miss_tokens":80,"prompt_cache_hit_tokens":20}}}"#,
    );
    assert!(matches!(
        event,
        Some(StreamEvent::MessageStart {
            input_tokens: Some(100),
            cache_creation_input_tokens: Some(80),
            cache_read_input_tokens: Some(20),
            ..
        })
    ));

    let event = parse_sse_event(
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"completion_tokens":25}}"#,
    );
    assert!(matches!(
        event,
        Some(StreamEvent::MessageDelta {
            output_tokens: 25,
            ..
        })
    ));
}

// --- SseStream tests ---

/// Helper: consume and assert the synthetic `HttpMeta` frame that
/// every `SseStream` now emits before any provider event. Keeps
/// the rest of the SSE body tests focused on protocol parsing
/// instead of the transport preamble.
async fn expect_http_meta<S>(stream: &mut SseStream<S>, expected_request_id: Option<&str>)
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    match stream.next().await {
        Some(Ok(StreamEvent::HttpMeta { request_id })) => {
            assert_eq!(request_id.as_deref(), expected_request_id);
        }
        other => panic!("Expected HttpMeta preamble, got {other:?}"),
    }
}

#[tokio::test]
async fn test_sse_stream_emits_http_meta_first() {
    let inner = bytes_stream(vec![
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    let mut stream = SseStream::with_request_id(inner, Some("req_01ABC".to_string()));
    // First event must be the synthetic HttpMeta, before any
    // provider event — otherwise `StreamAccumulator` can't seed
    // `provider_request_id` when `message_start` arrives.
    match stream.next().await {
        Some(Ok(StreamEvent::HttpMeta { request_id })) => {
            assert_eq!(request_id.as_deref(), Some("req_01ABC"));
        }
        other => panic!("Expected HttpMeta first, got {other:?}"),
    }
    // Next event is the actual provider frame.
    assert!(matches!(
        stream.next().await,
        Some(Ok(StreamEvent::MessageStop))
    ));
}

#[tokio::test]
async fn test_sse_stream_emits_http_meta_with_none_when_no_header() {
    let inner = bytes_stream(vec![
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    let mut stream = SseStream::new(inner);
    // `SseStream::new` leaves `request_id: None`; the preamble
    // still fires so consumers can pattern-match unconditionally.
    match stream.next().await {
        Some(Ok(StreamEvent::HttpMeta { request_id })) => {
            assert!(request_id.is_none());
        }
        other => panic!("Expected HttpMeta first, got {other:?}"),
    }
}

#[tokio::test]
async fn test_sse_stream_parses_complete_event() {
    let inner = bytes_stream(vec![
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let event = stream.next().await;
    assert!(matches!(event, Some(Ok(StreamEvent::MessageStop))));
    // stream should be finished
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_sse_stream_handles_partial_chunks() {
    let inner = bytes_stream(vec![
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
        "\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let event = stream.next().await;
    assert!(matches!(event, Some(Ok(StreamEvent::MessageStop))));
}

#[tokio::test]
async fn test_sse_stream_handles_crlf_delimiters() {
    let inner = bytes_stream(vec![
        "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let event = stream.next().await;
    assert!(matches!(event, Some(Ok(StreamEvent::MessageStop))));
}

#[tokio::test]
async fn test_sse_stream_emits_error_for_malformed_then_continues() {
    let inner = bytes_stream(vec![
        "event: unknown\ndata: {bad json}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let first = stream.next().await;
    assert!(
        matches!(&first, Some(Ok(StreamEvent::Error { message, .. })) if message.contains("malformed SSE JSON")),
        "expected StreamEvent::Error, got {first:?}"
    );
    // Error from malformed JSON marks finished because StreamEvent::Error sets finished=true
}

#[tokio::test]
async fn test_sse_stream_multiple_events() {
    let inner = bytes_stream(vec![
        "event: ping\ndata: {}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let first = stream.next().await;
    assert!(matches!(first, Some(Ok(StreamEvent::Ping))));
    let second = stream.next().await;
    assert!(matches!(second, Some(Ok(StreamEvent::MessageStop))));
}

#[tokio::test]
async fn test_sse_stream_empty_input() {
    let inner = bytes_stream(vec![]);
    let mut stream = SseStream::new(inner);
    // Empty upstream body still yields the HttpMeta preamble
    // (consumers rely on it as a deterministic first event), then
    // the stream terminates.
    expect_http_meta(&mut stream, None).await;
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_sse_stream_error_marks_finished() {
    let inner = bytes_stream(vec![
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n\n",
    ]);
    let mut stream = SseStream::new(inner);
    expect_http_meta(&mut stream, None).await;
    let event = stream.next().await;
    assert!(matches!(event, Some(Ok(StreamEvent::Error { .. }))));
    assert!(stream.next().await.is_none());
}
