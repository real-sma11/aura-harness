//! Visual block helpers for the inbound HTTP / WebSocket transcript.
//!
//! Symmetric to [`aura_agent::console`] / [`aura_reasoner::console`]:
//! emits multi-line "blocks" (`┌─ → GET /api/files` / `┌─ ← 401
//! unauthorized`) and one-line continuation rows under the
//! dedicated `"aura::console"` tracing target so the custom event
//! formatter in [`crate::console_format`] renders them verbatim.
//!
//! ## Why a dedicated module
//!
//! The outbound LLM-call transcript already pairs every `→ POST` with
//! either `← 200 OK` (success) or `← <status> <reason>` (failure).
//! On the inbound side, multiple rejection paths historically returned
//! a `StatusCode::into_response()` with no log at all (auth-middleware
//! 401, governor 429, body-limit 413, timeout 408, WS slot full,
//! oversized `SessionInit` open-then-close — see the comment on
//! [`crate::session::ws_handler::classify_ws_frame`]). This module
//! gives those rejections a shared visual surface so an operator
//! scanning a single log file sees the same `→ in` / `← out` pairing
//! for inbound traffic that already exists for outbound calls.
//!
//! ## Layout & colors
//!
//! Mirrors the outbound modules: row helper computes column widths,
//! borders / labels are dimmed, header carries a status-family color,
//! `colored` auto-disables when stdout is not a TTY and honors
//! `NO_COLOR`. The `wrap_row` / `human_*` helpers are a deliberate
//! third copy — the existing two copies (in `aura-agent::console` and
//! `aura-reasoner::console`) live next to their callers. A follow-up
//! can consolidate all three into a shared submodule of
//! [`crate::console_format`] once the surface settles.

use std::net::SocketAddr;

use colored::Colorize;
use tracing::info;

/// Tracing target the runtime's custom formatter renders verbatim.
/// Must stay in sync with [`crate::console_format::CONSOLE_TARGETS`]
/// and the matching constants in `aura-agent` / `aura-reasoner`.
pub const CONSOLE_TARGET: &str = "aura::console";

/// Width of the padded label column in [`wrap_row`].
const LABEL_WIDTH: usize = 14;

/// `│` (1) + three spaces (3) + padded label (14) + separator space (1) = 19.
const ROW_INDENT_COLS: usize = 1 + 3 + LABEL_WIDTH + 1;

/// Below this width fall back to single-line rendering rather than
/// emit a wrap that's mostly continuation prefix.
const MIN_VALUE_ROOM: usize = 16;

/// View into an inbound HTTP request — populated by the failure
/// observer middleware right before it emits the paired `→`/`←`
/// blocks. The summary block is only rendered on the failure path
/// (success requests stay quiet to keep transcript noise low).
pub struct InboundRequestView<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub peer: Option<SocketAddr>,
    /// Inbound `Content-Length` if the request reported one. `None`
    /// when the request had no body or used chunked transfer.
    pub body_bytes: Option<usize>,
}

/// View into the rejected inbound response. `reason` is a short stable
/// label (`"unauthorized"`, `"rate_limited"`, `"body_too_large"`,
/// `"timeout"`, `"upstream_5xx"`, `"error_<code>"`) the middleware
/// derives from the response status; renderer puts it in the header
/// and as a `class` row so an operator can scan either column.
pub struct InboundFailureView<'a> {
    pub status_code: u16,
    pub status_text: &'a str,
    pub reason: &'a str,
    pub elapsed_ms: u64,
    pub peer: Option<SocketAddr>,
    /// Optional response-body preview for handler-emitted error JSON
    /// (e.g. `ApiError::bad_request("...")`). Caller should already
    /// have collapsed control chars / clipped to a sane length.
    pub body_preview: Option<&'a str>,
}

/// Render the request half of an inbound rejection as a multi-line
/// block. Only emitted when the response was non-2xx.
pub fn inbound_request_summary_block(view: InboundRequestView<'_>) {
    let mut out = String::new();
    let header = format!("→ {} {}", view.method, view.path).cyan().bold();
    let tag = peer_tag(view.peer);
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    if let Some(bytes) = view.body_bytes {
        out.push_str(&wrap_row("body", &human_bytes(bytes)));
    }
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Render the response half of an inbound rejection. Header is
/// red-bold so a 401 / 429 / 5xx jumps out of the transcript.
pub fn inbound_failure_block(view: InboundFailureView<'_>) {
    let mut out = String::new();
    let header = format!("← {} {}", view.status_code, view.reason)
        .red()
        .bold();
    let tag = peer_tag(view.peer);
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    out.push_str(&wrap_row(
        "status",
        &format!(
            "{:<14} elapsed {:>7}",
            view.status_text,
            human_duration_ms(view.elapsed_ms)
        ),
    ));
    if let Some(body) = view.body_preview {
        let collapsed = collapse_for_row(body, 140);
        if !collapsed.is_empty() {
            out.push_str(&wrap_row("body", &collapsed));
        }
    }
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Render a single-line rejection notice for paths that don't have a
/// natural HTTP request/response shape — WebSocket upgrade refusals,
/// inbound-frame parse errors, oversized `SessionInit`, etc.
///
/// `scope` is a short tag like `"upgrade"`, `"upgrade.automaton"`,
/// `"framing"`. `reason` is a stable label that mirrors the error
/// `code` shipped to the client (so server / client log correlation
/// is trivial). `detail` is an optional free-form preview.
pub fn ws_rejection_line(scope: &str, reason: &str, detail: Option<&str>) {
    let detail_part = match detail {
        Some(d) if !d.is_empty() => format!(": {}", collapse_for_row(d, 140)),
        _ => String::new(),
    };
    let body = format!(
        "{} {} ws.{scope} {reason}{detail_part}",
        "│".dimmed(),
        "⊘".red().bold(),
    );
    info!(target: CONSOLE_TARGET, "{body}");
}

/// Map an HTTP status code to the short stable label rendered in
/// the failure block header. Lookup table kept here so the inbound
/// middleware and the WS-rejection paths share one vocabulary.
#[must_use]
pub fn reason_for_status(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        408 => "timeout",
        409 => "conflict",
        413 => "body_too_large",
        415 => "unsupported_media_type",
        422 => "unprocessable",
        429 => "rate_limited",
        500 => "server_error",
        501 => "not_implemented",
        502..=504 => "upstream_5xx",
        _ if (400..500).contains(&status) => "client_error",
        _ if (500..600).contains(&status) => "server_error",
        _ => "unknown",
    }
}

// ----------------------------------------------------------------------
// Formatting primitives — duplicated from the sibling modules; see the
// module docs for why.
// ----------------------------------------------------------------------

fn peer_tag(peer: Option<SocketAddr>) -> String {
    let body = match peer {
        Some(addr) => format!("[{addr}]"),
        None => String::from("[-]"),
    };
    body.cyan().dimmed().to_string()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 12345))
    }

    #[test]
    fn reason_for_status_known_codes() {
        assert_eq!(reason_for_status(401), "unauthorized");
        assert_eq!(reason_for_status(403), "forbidden");
        assert_eq!(reason_for_status(404), "not_found");
        assert_eq!(reason_for_status(408), "timeout");
        assert_eq!(reason_for_status(413), "body_too_large");
        assert_eq!(reason_for_status(429), "rate_limited");
        assert_eq!(reason_for_status(502), "upstream_5xx");
        assert_eq!(reason_for_status(418), "client_error");
        assert_eq!(reason_for_status(599), "server_error");
    }

    #[test]
    fn inbound_request_summary_block_renders_without_panic() {
        inbound_request_summary_block(InboundRequestView {
            method: "GET",
            path: "/api/files",
            peer: Some(loopback()),
            body_bytes: None,
        });
        inbound_request_summary_block(InboundRequestView {
            method: "POST",
            path: "/tx",
            peer: Some(loopback()),
            body_bytes: Some(2048),
        });
    }

    #[test]
    fn inbound_failure_block_renders_each_status_family() {
        inbound_failure_block(InboundFailureView {
            status_code: 401,
            status_text: "Unauthorized",
            reason: "unauthorized",
            elapsed_ms: 4,
            peer: Some(loopback()),
            body_preview: None,
        });
        inbound_failure_block(InboundFailureView {
            status_code: 429,
            status_text: "Too Many Requests",
            reason: "rate_limited",
            elapsed_ms: 1,
            peer: Some(loopback()),
            body_preview: None,
        });
        inbound_failure_block(InboundFailureView {
            status_code: 413,
            status_text: "Payload Too Large",
            reason: "body_too_large",
            elapsed_ms: 1,
            peer: Some(loopback()),
            body_preview: Some("body too large"),
        });
        inbound_failure_block(InboundFailureView {
            status_code: 408,
            status_text: "Request Timeout",
            reason: "timeout",
            elapsed_ms: 30_000,
            peer: Some(loopback()),
            body_preview: None,
        });
        inbound_failure_block(InboundFailureView {
            status_code: 502,
            status_text: "Bad Gateway",
            reason: "upstream_5xx",
            elapsed_ms: 87,
            peer: None,
            body_preview: None,
        });
    }

    #[test]
    fn ws_rejection_line_renders_each_scope() {
        ws_rejection_line("upgrade", "slot_full", Some("cap=128"));
        ws_rejection_line(
            "upgrade.automaton",
            "unauthorized",
            Some("automaton_id=foo"),
        );
        ws_rejection_line(
            "framing",
            "parse_error",
            Some("expected `,` or `}` at line 1 column 4"),
        );
        ws_rejection_line("framing", "transport_error", None);
        ws_rejection_line("framing", "not_initialized", None);
    }

    #[test]
    fn collapse_for_row_strips_control_and_whitespace() {
        let raw = "line1\nline2\r\nline3 ";
        let collapsed = collapse_for_row(raw, 200);
        assert!(!collapsed.contains('\n'));
        assert!(!collapsed.contains('\r'));
        assert_eq!(collapsed, "line1 line2 line3");
    }

    #[test]
    fn collapse_for_row_truncates_with_ellipsis() {
        let raw = "x".repeat(200);
        let collapsed = collapse_for_row(&raw, 50);
        assert!(collapsed.ends_with('…'));
        assert_eq!(collapsed.chars().count(), 51);
    }
}
