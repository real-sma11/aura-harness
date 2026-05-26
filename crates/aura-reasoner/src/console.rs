//! Visual block helpers for Anthropic request / response logging.
//!
//! These mirror the structure in `aura-agent::console`: every block is
//! a self-contained multi-line string emitted through `tracing::info!`
//! with the dedicated `"aura::console"` target, so the custom event
//! formatter in `aura-runtime` can render it verbatim without level /
//! target / field noise.
//!
//! The forensic per-field `INFO` logs in
//! `crates/aura-reasoner/src/anthropic/provider.rs` were demoted to
//! `debug!` as part of the same change — operators get a clean
//! transcript by default and can opt back into the full field dump
//! with `RUST_LOG=aura_reasoner=debug`.

use tracing::info;

use crate::types::ModelResponse;

/// Tracing target the runtime's custom formatter renders verbatim.
pub const CONSOLE_TARGET: &str = "aura::console";

/// View into the request data the multi-line block needs. Borrowing
/// avoids a string-clone storm at the call site (the values come
/// directly from the existing `RequestDiagnosticsSummary` plus a few
/// derived labels).
pub struct AnthropicRequestView<'a> {
    pub model: &'a str,
    pub kind: &'a str,
    pub body_bytes: usize,
    pub messages_count: usize,
    pub tools_count: usize,
    pub tool_choice: &'a str,
    pub thinking_label: &'a str,
    pub system_bytes: usize,
    pub last_user_bytes: usize,
    pub last_user_hash: Option<&'a str>,
    pub headers_present: &'a str,
    pub request_hash: &'a str,
}

/// Mirror view for the response side. Populated from the wire status,
/// `response.usage`, and the wall-clock elapsed time the provider
/// already measures.
pub struct AnthropicResponseView<'a> {
    pub status_code: u16,
    pub status_text: &'a str,
    pub stop_reason: &'a str,
    pub elapsed_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub request_id: Option<&'a str>,
}

/// Render the request half of an Anthropic call as a multi-line block.
pub fn anthropic_request_block(req: AnthropicRequestView<'_>) {
    let mut out = String::new();
    out.push_str("┌─ → POST /v1/messages\n");
    out.push_str(&row("model", &format!("{:<24} kind  {}", req.model, req.kind)));
    out.push_str(&row(
        "body",
        &format!(
            "{:<14} msgs {:<3}    tools {} ({})",
            human_bytes(req.body_bytes),
            req.messages_count,
            req.tools_count,
            req.tool_choice
        ),
    ));
    out.push_str(&row(
        "thinking",
        &format!(
            "{:<14} system {:<6} last_user {} / {}",
            req.thinking_label,
            human_bytes(req.system_bytes),
            human_bytes(req.last_user_bytes),
            req.last_user_hash.unwrap_or("-"),
        ),
    ));
    out.push_str(&row(
        "headers",
        &format!(
            "{:<14} request_hash {}",
            req.headers_present, req.request_hash
        ),
    ));
    out.push_str("└─");
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Convenience wrapper: take a fully-parsed [`ModelResponse`] plus a
/// wall-clock latency and emit the matching response block. Shared
/// between [`crate::AnthropicProvider::complete`] and the streaming
/// finalizer in `aura-agent::agent_loop::streaming`.
pub fn emit_response_block(
    response: &ModelResponse,
    elapsed_ms: u64,
    status_code: u16,
    status_text: &str,
) {
    let stop_label = format!("{:?}", response.stop_reason);
    anthropic_response_block(AnthropicResponseView {
        status_code,
        status_text,
        stop_reason: &stop_label,
        elapsed_ms,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        request_id: response.trace.provider_request_id.as_deref(),
    });
}

/// Render the response half of an Anthropic call symmetric to
/// [`anthropic_request_block`].
pub fn anthropic_response_block(resp: AnthropicResponseView<'_>) {
    let mut out = String::new();
    out.push_str(&format!("┌─ ← {} {}\n", resp.status_code, resp.status_text));
    out.push_str(&row(
        "stop",
        &format!(
            "{:<14} elapsed {:>7}",
            resp.stop_reason,
            human_duration_ms(resp.elapsed_ms)
        ),
    ));
    out.push_str(&row(
        "tokens",
        &format!("in {} / out {}", resp.input_tokens, resp.output_tokens),
    ));
    if let Some(req_id) = resp.request_id {
        out.push_str(&row("request_id", req_id));
    }
    out.push_str("└─");
    info!(target: CONSOLE_TARGET, "{out}");
}

// ----------------------------------------------------------------------
// Formatting primitives
// ----------------------------------------------------------------------

fn row(label: &str, value: &str) -> String {
    format!("│   {label:<14} {value}\n")
}

fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        let kb = n as f64 / 1024.0;
        format!("{kb:.1} KB")
    } else {
        let mb = n as f64 / (1024.0 * 1024.0);
        format!("{mb:.2} MB")
    }
}

fn human_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        let s = ms as f64 / 1000.0;
        format!("{s:.2} s")
    } else {
        let m = ms / 60_000;
        let s = (ms % 60_000) as f64 / 1000.0;
        format!("{m}m {s:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.00 MB");
    }

    #[test]
    fn human_duration_units() {
        assert_eq!(human_duration_ms(250), "250 ms");
        assert_eq!(human_duration_ms(4200), "4.20 s");
        assert_eq!(human_duration_ms(61_500), "1m 1.5s");
    }

    #[test]
    fn request_block_renders_without_panic() {
        anthropic_request_block(AnthropicRequestView {
            model: "claude-opus-4-6",
            kind: "DevLoopContinuation",
            body_bytes: 24_656,
            messages_count: 7,
            tools_count: 15,
            tool_choice: "auto",
            thinking_label: "on(b=1024)",
            system_bytes: 3823,
            last_user_bytes: 932,
            last_user_hash: Some("8c65c4f3"),
            headers_present: "ver auth ct beta proj agent sess org",
            request_hash: "1e62bdae",
        });
    }

    #[test]
    fn response_block_renders_without_panic() {
        anthropic_response_block(AnthropicResponseView {
            status_code: 200,
            status_text: "OK",
            stop_reason: "ToolUse",
            elapsed_ms: 4_200,
            input_tokens: 12_345,
            output_tokens: 234,
            request_id: Some("req_01abcd"),
        });
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod additional_tests {
    // Placeholder: future tests for emit_response_block can live here
    // once `crate::types::ModelResponse` becomes easily constructible
    // outside of integration paths.
}
