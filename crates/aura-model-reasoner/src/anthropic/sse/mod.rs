//! Server-Sent Events (SSE) streaming wrapper for the Anthropic
//! provider's `messages` endpoint.
//!
//! Phase 2b split the original `sse.rs` into focused submodules:
//!
//! - [`mod@parse`] — line-buffered SSE parsing primitives. Owns the
//!   `\n\n` / `\r\n\r\n` event-boundary scanner ([`pop_event_block`])
//!   and the per-block `event:` / `data:` line splitter
//!   ([`parse_sse_event`]).
//! - [`mod@event`] — Anthropic stream event-type translation. Maps the
//!   wire-protocol [`super::api_types::SseEvent`] / `SseDelta` /
//!   `SseContentBlock` enums into the public-facing
//!   [`crate::StreamEvent`].
//! - [`mod@state`] — buffered state machine [`SseStream`]. Wraps a
//!   byte stream, surfaces the synthetic `HttpMeta` preamble, and
//!   feeds [`parse::pop_event_block`] / [`parse::parse_sse_event`] to
//!   emit `StreamEvent`s.
//! - `tests` — unit tests covering parser edge cases (CRLF, partial
//!   chunks, malformed payloads) and the `HttpMeta` preamble
//!   guarantees.
//!
//! [`pop_event_block`]: parse::pop_event_block
//! [`parse_sse_event`]: parse::parse_sse_event

mod event;
mod parse;
mod state;

#[cfg(test)]
mod tests;

pub(super) use state::SseStream;
