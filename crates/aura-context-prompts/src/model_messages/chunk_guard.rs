//! `[CHUNK_GUARD]` body emitted when `write_file` content exceeds
//! [`aura_config::WRITE_FILE_CHUNK_BYTES`].
//!
//! The agent loop's `tool_pipeline::partition_oversized_writes` calls
//! [`render_chunk_guard_body`] to produce both the `side_messages`
//! warning prose AND the `[CHUNK_GUARD] <body>` tool-result content
//! (the `[CHUNK_GUARD]` prefix is appended at the call site).

/// Render the warning body the chunk guard emits when a `write_file`
/// `content` field exceeds the per-turn cap.
///
/// `actual_bytes` is the offending content's byte length; `cap_bytes`
/// is [`aura_config::WRITE_FILE_CHUNK_BYTES`] threaded through so the
/// rendered message names the same number the guard enforced.
#[must_use]
pub fn render_chunk_guard_body(actual_bytes: usize, cap_bytes: usize) -> String {
    format!(
        "`write_file` content of {actual_bytes} bytes exceeds the {cap_bytes}-byte per-turn cap. \
         Next turn: call `write_file` with only the module-doc + imports + one stub \
         (≤{cap_bytes} bytes), then use `edit_file` appends for the rest."
    )
}

/// Static prefix prepended to the rendered body when it is sent to
/// the model as a `tool_result` `content`. Pulled out as a const so
/// the guardrail tests can pin the literal.
pub const CHUNK_GUARD_TAG: &str = "[CHUNK_GUARD] ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_carries_actual_and_cap_bytes() {
        let body = render_chunk_guard_body(40_000, 32_000);
        assert!(body.contains("40000 bytes"));
        assert!(body.contains("32000-byte"));
        assert!(body.contains("module-doc + imports + one stub"));
    }
}
