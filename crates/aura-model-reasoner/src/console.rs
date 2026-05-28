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
//!
//! ## Layout & colors
//!
//! See the matching module doc on `aura-agent::console` for the
//! shared layout / color philosophy. The wrap helper here is a
//! deliberate duplicate of the one in `aura-agent` — both crates
//! render block rows independently, so the helper lives next to its
//! callers rather than being promoted to a shared crate. A follow-up
//! can consolidate both copies into `aura-runtime::console_format`
//! once the shared surface settles.

use colored::Colorize;
use tracing::info;

use crate::types::ModelResponse;

/// Tracing target the runtime's custom formatter renders verbatim.
pub const CONSOLE_TARGET: &str = "aura::console";

/// Width of the padded label column in [`wrap_row`].
const LABEL_WIDTH: usize = 14;

/// Display columns before the value chunk starts:
/// `│` (1) + three spaces (3) + padded label (14) + separator space (1) = 19.
const ROW_INDENT_COLS: usize = 1 + 3 + LABEL_WIDTH + 1;

/// Minimum value column room before wrap is attempted. Narrower
/// terminals fall back to single-line rendering rather than emit a
/// wrap that's mostly continuation prefix.
const MIN_VALUE_ROOM: usize = 16;

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
    /// Comma-joined list of tool names attached to the request, taken
    /// straight from the serialized body. Empty when the request
    /// shipped without any tools (e.g. `kind: Auxiliary` sub-prompts);
    /// the renderer skips the `tool_names` row in that case so the
    /// block stays tight.
    pub tool_names: &'a str,
    pub thinking_label: &'a str,
    pub system_bytes: usize,
    pub last_user_bytes: usize,
    pub last_user_hash: Option<&'a str>,
    pub headers_present: &'a str,
    pub request_hash: &'a str,
    /// Short semantic label for the network destination (e.g.
    /// `"aura-network"` for any call routed through the LLM proxy).
    pub destination: &'a str,
    /// Host portion of the target URL (e.g.
    /// `"aura-router.onrender.com"`).
    pub destination_host: &'a str,
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
    /// Short semantic label for the network destination (e.g.
    /// `"aura-network"`). Host is omitted on the response side to
    /// keep the header line scannable — the request header above
    /// already carries the full host.
    pub destination: &'a str,
}

/// Render the request half of an Anthropic call as a multi-line block.
pub fn anthropic_request_block(req: &AnthropicRequestView<'_>) {
    let mut out = String::new();
    let header = "→ POST /v1/messages".cyan().bold();
    let tag = destination_tag(req.destination, Some(req.destination_host));
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    out.push_str(&wrap_row("model", req.model));
    out.push_str(&wrap_row("kind", req.kind));
    out.push_str(&wrap_row("body", &human_bytes(req.body_bytes)));
    out.push_str(&wrap_row("msgs", &req.messages_count.to_string()));
    out.push_str(&wrap_row(
        "tools",
        &format!("{} ({})", req.tools_count, req.tool_choice),
    ));
    if !req.tool_names.is_empty() {
        out.push_str(&wrap_row("tool_names", req.tool_names));
    }
    out.push_str(&wrap_row(
        "thinking",
        // Widen the inline padding to fit the richer label format
        // (e.g. `on(medium · enabled · b=4096)`); the legacy 14-col
        // pad was sized for `on` / `off` and would smash the system
        // / last_user tail against the label on every thinking turn.
        &format!(
            "{:<32} system {:<6} last_user {} / {}",
            req.thinking_label,
            human_bytes(req.system_bytes),
            human_bytes(req.last_user_bytes),
            req.last_user_hash.unwrap_or("-"),
        ),
    ));
    out.push_str(&wrap_row("headers", req.headers_present));
    out.push_str(&wrap_row("request_hash", req.request_hash));
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Convenience wrapper: take a fully-parsed [`ModelResponse`] plus a
/// wall-clock latency and emit the matching response block. Shared
/// between [`crate::AnthropicProvider::complete`] and the streaming
/// finalizer in `aura-agent::agent_loop::streaming`.
///
/// All callers in the harness target the LLM proxy, so the
/// destination is hard-coded to `"aura-network"` here. If a future
/// caller targets a different service tier, lift this to an explicit
/// parameter.
pub fn emit_response_block(
    response: &ModelResponse,
    elapsed_ms: u64,
    status_code: u16,
    status_text: &str,
) {
    let stop_label = format!("{:?}", response.stop_reason);
    anthropic_response_block(&AnthropicResponseView {
        status_code,
        status_text,
        stop_reason: &stop_label,
        elapsed_ms,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        request_id: response.trace.provider_request_id.as_deref(),
        destination: "aura-network",
    });
}

/// Render the response half of an Anthropic call symmetric to
/// [`anthropic_request_block`].
pub fn anthropic_response_block(resp: &AnthropicResponseView<'_>) {
    let mut out = String::new();
    let header_text = format!("← {} {}", resp.status_code, resp.status_text);
    let header = match resp.status_code {
        200..=299 => header_text.green().bold(),
        300..=399 => header_text.yellow().bold(),
        _ => header_text.red().bold(),
    };
    let tag = destination_tag(resp.destination, None);
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    out.push_str(&wrap_row(
        "stop",
        &format!(
            "{:<14} elapsed {:>7}",
            resp.stop_reason,
            human_duration_ms(resp.elapsed_ms)
        ),
    ));
    out.push_str(&wrap_row(
        "tokens",
        &format!("in {} / out {}", resp.input_tokens, resp.output_tokens),
    ));
    if let Some(req_id) = resp.request_id {
        out.push_str(&wrap_row("request_id", req_id));
    }
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// View into a blocked / failed `/v1/messages` round-trip. `status_code`
/// is `None` for failures that never produced an HTTP response (transport
/// timeout, DNS failure, body-serialize bug); for those the renderer
/// substitutes a `← transport failed` header.
///
/// Distinct from [`AnthropicResponseView`] so failure-only rows
/// (`class`, `request_id`, `retry_after`, `body`) do not have to fight
/// success-only rows (`tokens`) for column space.
pub struct AnthropicFailureView<'a> {
    pub status_code: Option<u16>,
    pub status_text: &'a str,
    /// Stable short class label: `cloudflare_block` / `rate_limited_429`
    /// / `upstream_5xx` / `insufficient_credits` / `transport` / `parse`
    /// / `sse_transport` / `other`. Mirrors the strings emitted by
    /// `retry_reason_for` plus a few non-HTTP buckets so an operator
    /// scanning the visual transcript can pivot to `retries.jsonl` by
    /// the same key.
    pub class: &'a str,
    pub elapsed_ms: u64,
    pub request_id: Option<&'a str>,
    pub retry_after_s: Option<u64>,
    /// Already-truncated body / error preview (≤140 chars recommended).
    /// Renderer collapses control chars + interior whitespace.
    pub body_preview: Option<&'a str>,
    pub destination: &'a str,
}

/// Planned next action the retry classifier picked for a failed
/// attempt. Rendered as a one-line `↻ retry …` / `→ fallback …` /
/// `✗ propagate …` continuation under the failure block so the
/// transcript shows what the harness will do about the block.
pub enum RetryDecisionView<'a> {
    Retry {
        attempt_that_failed: u32,
        max_retries: u32,
        sleep_ms: u64,
        body_cap_bytes: Option<usize>,
    },
    Fallback {
        next_model: &'a str,
    },
    Propagate {
        reason: &'a str,
    },
}

/// Render the failure half of an Anthropic call as a multi-line block.
/// Symmetric to [`anthropic_response_block`]: same destination tag,
/// same row layout, same target — operators reading the transcript
/// see one paired box per round-trip regardless of outcome.
pub fn anthropic_failure_block(view: &AnthropicFailureView<'_>) {
    let mut out = String::new();
    let header_text = match view.status_code {
        Some(code) => format!("← {} {}", code, view.status_text),
        None => format!("← {} ({})", view.status_text, view.class),
    };
    let header = header_text.red().bold();
    let tag = destination_tag(view.destination, None);
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    out.push_str(&wrap_row(
        "class",
        &format!(
            "{:<14} elapsed {:>7}",
            view.class,
            human_duration_ms(view.elapsed_ms)
        ),
    ));
    if let Some(req_id) = view.request_id {
        out.push_str(&wrap_row("request_id", req_id));
    }
    if let Some(secs) = view.retry_after_s {
        out.push_str(&wrap_row("retry_after", &format!("{secs}s")));
    }
    if let Some(body) = view.body_preview {
        let collapsed = collapse_for_row(body, 140);
        if !collapsed.is_empty() {
            out.push_str(&wrap_row("body", &collapsed));
        }
    }
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Render the retry classifier's planned next action as a single
/// continuation line — sits between the failure block and the next
/// request block so the transcript shows what the harness will do
/// about the block without hunting through `warn!` lines.
pub fn anthropic_retry_decision_line(decision: &RetryDecisionView<'_>) {
    let body = match *decision {
        RetryDecisionView::Retry {
            attempt_that_failed,
            max_retries,
            sleep_ms,
            body_cap_bytes,
        } => {
            let cap =
                body_cap_bytes.map_or_else(String::new, |c| format!(" · cap {}", human_bytes(c)));
            // Attempt counts are 1-based for human readability:
            // `attempt_that_failed` is the 0-based index of the call
            // that just failed, so the *next* attempt is +2 of N.
            format!(
                "{}   {} retry {}/{} in {}{}",
                "│".dimmed(),
                "↻".yellow().bold(),
                attempt_that_failed.saturating_add(2),
                max_retries.saturating_add(1),
                human_duration_ms(sleep_ms),
                cap,
            )
        }
        RetryDecisionView::Fallback { next_model } => {
            format!(
                "{}   {} fallback to {next_model}",
                "│".dimmed(),
                "→".yellow().bold(),
            )
        }
        RetryDecisionView::Propagate { reason } => {
            format!(
                "{}   {} propagate ({reason})",
                "│".dimmed(),
                "✗".red().bold(),
            )
        }
    };
    info!(target: CONSOLE_TARGET, "{body}");
}

/// Derive a short host label from a base URL (e.g.
/// `"https://aura-router.onrender.com"` → `"aura-router.onrender.com"`).
/// Returns the input unchanged when no scheme prefix is found.
#[must_use]
pub fn extract_host(base_url: &str) -> &str {
    let after_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    after_scheme.split('/').next().unwrap_or(after_scheme)
}

// ----------------------------------------------------------------------
// Formatting primitives
// ----------------------------------------------------------------------

/// Render `[destination · host]` (host optional) styled as dim cyan
/// so it sits subtly next to the bold header text.
fn destination_tag(destination: &str, host: Option<&str>) -> String {
    let body = match host {
        Some(h) if !h.is_empty() => format!("[{destination} · {h}]"),
        _ => format!("[{destination}]"),
    };
    body.cyan().dimmed().to_string()
}

/// Render a labeled row, wrapping long values so continuation lines
/// stay indented under the value column (column [`ROW_INDENT_COLS`]).
/// See `aura-agent::console::wrap_row` for the design notes — this
/// is a deliberate duplicate kept next to its callers.
fn wrap_row(label: &str, value: &str) -> String {
    let padded_label = format!("{label:<LABEL_WIDTH$}");
    let first_prefix = format!("{}   {} ", "│".dimmed(), padded_label.bold());
    let cont_prefix = format!("{}{}", "│".dimmed(), " ".repeat(ROW_INDENT_COLS - 1));

    let Some(term_width) = detect_term_width() else {
        return format!("{first_prefix}{value}\n");
    };
    if term_width <= ROW_INDENT_COLS + MIN_VALUE_ROOM {
        return format!("{first_prefix}{value}\n");
    }
    let value_room = term_width - ROW_INDENT_COLS;

    if value.chars().count() <= value_room {
        return format!("{first_prefix}{value}\n");
    }

    let chunks = wrap_value_chunks(value, value_room);
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            out.push_str(&first_prefix);
        } else {
            out.push_str(&cont_prefix);
        }
        out.push_str(chunk);
        out.push('\n');
    }
    out
}

fn detect_term_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize)
}

fn wrap_value_chunks(value: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![value.to_string()];
    }
    let chars: Vec<char> = value.chars().collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let remaining = chars.len() - start;
        if remaining <= max {
            chunks.push(chars[start..].iter().collect());
            break;
        }
        let window_end = start + max;
        let split_at = chars[start..window_end]
            .iter()
            .rposition(|c| *c == ',')
            .map(|pos| start + pos + 1)
            .or_else(|| {
                chars[start..window_end]
                    .iter()
                    .rposition(|c| c.is_whitespace())
                    .map(|pos| start + pos + 1)
            })
            .unwrap_or(window_end);
        let split_at = split_at.max(start + 1);
        let chunk: String = chars[start..split_at].iter().collect();
        chunks.push(chunk.trim_end().to_string());
        let mut next = split_at;
        while next < chars.len() && chars[next].is_whitespace() {
            next += 1;
        }
        start = next;
    }
    chunks
}

fn human_bytes(n: usize) -> String {
    // Lossy `usize as f64` casts are intentional here: this helper
    // only formats orders of magnitude on a transcript so a
    // sub-mantissa rounding error on truly huge values is
    // indistinguishable in the rendered output.
    #[allow(clippy::cast_precision_loss)]
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

/// Collapse control chars + interior whitespace into a single inline
/// preview. Used for failure-block `body` rows where the upstream
/// response (e.g. a Cloudflare HTML page) would otherwise smash the
/// box layout if rendered verbatim.
fn collapse_for_row(content: &str, limit: usize) -> String {
    let collapsed: String = content
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = collapsed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "'");
    if trimmed.chars().count() <= limit {
        trimmed
    } else {
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}…")
    }
}

fn human_duration_ms(ms: u64) -> String {
    // `u64 as f64` is precision-losing for ~292 million-year wall
    // clocks but the values formatted here are bounded by network
    // request timeouts; the rendered output is unaffected.
    #[allow(clippy::cast_precision_loss)]
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
    fn extract_host_strips_scheme_and_path() {
        assert_eq!(
            extract_host("https://aura-router.onrender.com"),
            "aura-router.onrender.com"
        );
        assert_eq!(
            extract_host("http://localhost:8080/v1/messages"),
            "localhost:8080"
        );
        assert_eq!(
            extract_host("aura-router.onrender.com"),
            "aura-router.onrender.com"
        );
    }

    #[test]
    fn destination_tag_renders_host_when_present() {
        colored::control::set_override(false);
        let tag = destination_tag("aura-network", Some("aura-router.onrender.com"));
        assert_eq!(tag, "[aura-network · aura-router.onrender.com]");
        let tag = destination_tag("aura-network", None);
        assert_eq!(tag, "[aura-network]");
        colored::control::unset_override();
    }

    #[test]
    fn wrap_value_breaks_on_comma_preference() {
        let chunks = wrap_value_chunks("aaa,bbb,ccc,ddd,eee", 12);
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(
                chunk.ends_with(','),
                "expected comma-terminated chunk, got {chunk:?}"
            );
        }
        assert_eq!(chunks.join(""), "aaa,bbb,ccc,ddd,eee");
    }

    #[test]
    fn wrap_value_breaks_on_whitespace_when_no_comma() {
        let chunks = wrap_value_chunks("alpha beta gamma delta epsilon", 12);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 12);
            assert!(!chunk.starts_with(' '));
        }
    }

    #[test]
    fn wrap_row_no_tty_returns_single_line() {
        colored::control::set_override(false);
        let out = wrap_row("model", "claude-opus-4-6");
        assert_eq!(out, "│   model          claude-opus-4-6\n");
        colored::control::unset_override();
    }

    #[test]
    fn request_block_renders_without_panic() {
        anthropic_request_block(&AnthropicRequestView {
            model: "claude-opus-4-6",
            kind: "DevLoopContinuation",
            body_bytes: 24_656,
            messages_count: 7,
            tools_count: 15,
            tool_choice: "auto",
            tool_names: "Read,Write,Edit,Glob,Grep,Shell,SemanticSearch",
            thinking_label: "on(medium · enabled · b=4096)",
            system_bytes: 3823,
            last_user_bytes: 932,
            last_user_hash: Some("8c65c4f3"),
            headers_present: "ver auth ct beta proj agent sess org",
            request_hash: "1e62bdae",
            destination: "aura-network",
            destination_host: "aura-router.onrender.com",
        });
    }

    #[test]
    fn request_block_skips_tool_names_when_empty() {
        // Auxiliary-style request (no tools attached): we still want
        // the block to render, but the `tool_names` row should not
        // appear since there's nothing to enumerate. Asserting via
        // direct construction here — the renderer logs to a tracing
        // target so this is a smoke test that no panic / format
        // misalignment fires when the field is empty.
        anthropic_request_block(&AnthropicRequestView {
            model: "claude-opus-4-6",
            kind: "Auxiliary",
            body_bytes: 12_345,
            messages_count: 1,
            tools_count: 0,
            tool_choice: "n/a",
            tool_names: "",
            thinking_label: "off",
            system_bytes: 162,
            last_user_bytes: 14_700,
            last_user_hash: Some("511e97e8"),
            headers_present: "ver auth ct beta proj agent sess org",
            request_hash: "d5d47e97",
            destination: "aura-network",
            destination_host: "aura-router.onrender.com",
        });
    }

    #[test]
    fn response_block_renders_without_panic() {
        anthropic_response_block(&AnthropicResponseView {
            status_code: 200,
            status_text: "OK",
            stop_reason: "ToolUse",
            elapsed_ms: 4_200,
            input_tokens: 12_345,
            output_tokens: 234,
            request_id: Some("req_01abcd"),
            destination: "aura-network",
        });
    }

    #[test]
    fn failure_block_renders_403_cloudflare() {
        anthropic_failure_block(&AnthropicFailureView {
            status_code: Some(403),
            status_text: "Forbidden",
            class: "cloudflare_block",
            elapsed_ms: 612,
            request_id: Some("cf-ray-deadbeef"),
            retry_after_s: None,
            body_preview: Some(
                "<!DOCTYPE html>\n<html><body>\nyour request was blocked\n</body></html>",
            ),
            destination: "aura-network",
        });
    }

    #[test]
    fn failure_block_renders_429_with_retry_after() {
        anthropic_failure_block(&AnthropicFailureView {
            status_code: Some(429),
            status_text: "Too Many Requests",
            class: "rate_limited_429",
            elapsed_ms: 84,
            request_id: Some("req_xxx"),
            retry_after_s: Some(7),
            body_preview: Some("{\"error\":{\"code\":\"RATE_LIMITED\"}}"),
            destination: "aura-network",
        });
    }

    #[test]
    fn failure_block_renders_transport() {
        anthropic_failure_block(&AnthropicFailureView {
            status_code: None,
            status_text: "transport failed",
            class: "transport",
            elapsed_ms: 30_000,
            request_id: None,
            retry_after_s: None,
            body_preview: Some("connection reset by peer"),
            destination: "aura-network",
        });
    }

    #[test]
    fn failure_block_renders_parse() {
        anthropic_failure_block(&AnthropicFailureView {
            status_code: Some(200),
            status_text: "OK",
            class: "parse",
            elapsed_ms: 1_840,
            request_id: Some("req_parse"),
            retry_after_s: None,
            body_preview: Some("expected `,` or `}` at line 1 column 87"),
            destination: "aura-network",
        });
    }

    #[test]
    fn retry_decision_line_renders_each_variant() {
        anthropic_retry_decision_line(&RetryDecisionView::Retry {
            attempt_that_failed: 0,
            max_retries: 2,
            sleep_ms: 1500,
            body_cap_bytes: Some(192 * 1024),
        });
        anthropic_retry_decision_line(&RetryDecisionView::Retry {
            attempt_that_failed: 1,
            max_retries: 2,
            sleep_ms: 3000,
            body_cap_bytes: None,
        });
        anthropic_retry_decision_line(&RetryDecisionView::Fallback {
            next_model: "claude-haiku-4-6",
        });
        anthropic_retry_decision_line(&RetryDecisionView::Propagate {
            reason: "retries exhausted",
        });
    }

    #[test]
    fn collapse_for_row_strips_control_and_whitespace() {
        let raw = "<!DOCTYPE html>\n  <html>\r\n  <body>  blocked  </body>\n</html>";
        let collapsed = collapse_for_row(raw, 200);
        assert!(!collapsed.contains('\n'));
        assert!(!collapsed.contains('\r'));
        assert!(!collapsed.contains("  "));
        assert!(collapsed.contains("<html> <body> blocked </body> </html>"));
    }

    #[test]
    fn collapse_for_row_truncates_with_ellipsis() {
        let raw = "a".repeat(200);
        let collapsed = collapse_for_row(&raw, 50);
        assert!(collapsed.ends_with('…'));
        assert_eq!(collapsed.chars().count(), 51);
    }
}

#[cfg(test)]
#[allow(dead_code)]
mod additional_tests {
    // Placeholder: future tests for emit_response_block can live here
    // once `crate::types::ModelResponse` becomes easily constructible
    // outside of integration paths.
}
