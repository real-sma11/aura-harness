//! Visual block helpers for the structured console transcript.
//!
//! These helpers render the task → turn → sampling topology and the
//! per-iteration tool-execution batches as multi-line "blocks" with
//! light box-drawing rules so an operator can scan a single log file
//! and immediately see where one sampling request ends and the next
//! begins.
//!
//! Every block is emitted through `tracing::info!` with a dedicated
//! `"aura::console"` target so the custom event formatter in
//! `crates/aura-runtime/src/console_format.rs` can render the
//! message verbatim — without level / target / field noise — while
//! every other log line keeps the default compact format.
//!
//! ## Layout
//!
//! Standard rows use [`wrap_row`], which renders
//! `│   <label padded to 14> <value>` and pre-wraps long values so
//! that continuation lines start at column 19 (directly under the
//! value column) instead of letting the terminal soft-wrap them back
//! to column 0. The wrap is gated on TTY detection — when stdout is
//! not a terminal (e.g. piped to a file) the helper falls back to a
//! single-line render so log files stay byte-identical to the pre-
//! wrap layout.
//!
//! ## Colors
//!
//! Borders / labels / header lines are styled with [`colored`].
//! `colored` auto-disables when stdout is not a TTY and honors the
//! `NO_COLOR` env var, so colorization is opt-out for free on
//! redirected output. Value tokens are deliberately *not* colored
//! inline — that would inject ANSI escape bytes into the value
//! string and confuse [`wrap_row`]'s column math.

use std::time::Duration;

use aura_model_reasoner::StreamPhase;
use colored::Colorize;
use tracing::info;

use crate::types::{ToolCallInfo, ToolCallResult};

/// Tracing target the custom formatter recognises and renders verbatim.
pub const CONSOLE_TARGET: &str = "aura::console";

/// Width of the padded label column in [`wrap_row`].
const LABEL_WIDTH: usize = 14;

/// Display columns before the value chunk starts:
/// `│` (1) + three spaces (3) + padded label (14) + separator space (1) = 19.
const ROW_INDENT_COLS: usize = 1 + 3 + LABEL_WIDTH + 1;

/// Minimum value column room before wrap is even attempted. If the
/// terminal is narrower than this, fall back to single-line rendering
/// rather than emit a wrap that's mostly continuation prefix.
const MIN_VALUE_ROOM: usize = 16;

/// Render `─── task <8> turn N sampling I ───` as a separator at the
/// top of every sampling request. Emitted by
/// [`crate::agent_loop::sampling::run_sampling_request`].
pub fn sampling_boundary(task_id: &str, turn: u32, iter: usize) {
    let short = short_task_id(task_id);
    let line =
        format!("─── task {short} · turn {turn} · sampling {iter} ──────────────────────────────");
    info!(target: CONSOLE_TARGET, "{}", line.bright_cyan());
}

/// Render the request half of an Anthropic call as a multi-line block.
/// Field arguments are passed verbatim and trusted; the helper does
/// not redact or truncate.
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
    out.push_str(&wrap_row(
        "thinking",
        &format!(
            "{:<14} system {:<6} last_user {} / {}",
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

/// Render the "tools requested + executed" block from a tool-use
/// stop reason. Each tool gets one line with a status glyph
/// (`✓` success, `✗` error, `↺` cached) so the operator can spot
/// failed calls without scanning fields.
pub fn tools_block(
    calls: &[ToolCallInfo],
    results: &[ToolCallResult],
    cached_ids: &std::collections::HashSet<String>,
    blocked_ids: &std::collections::HashSet<String>,
) {
    let executed = calls.len() - cached_ids.len();
    let mut out = String::new();
    let header = format!(
        "tools  ({} requested · {} cached · {} to execute)",
        calls.len(),
        cached_ids.len(),
        executed,
    );
    out.push_str(&format!("{} {}\n", "┌─".dimmed(), header.blue().bold(),));

    for call in calls {
        let result = results.iter().find(|r| r.tool_use_id == call.id);
        let (glyph, glyph_color): (char, colored::Color) = match result {
            None => ('·', colored::Color::BrightBlack),
            Some(r) if r.is_error => ('✗', colored::Color::Red),
            Some(_) if cached_ids.contains(&call.id) => ('↺', colored::Color::Yellow),
            Some(_) if blocked_ids.contains(&call.id) => ('⊘', colored::Color::Yellow),
            Some(_) => ('✓', colored::Color::Green),
        };
        let short_id = short_tool_use_id(&call.id);
        let len_str = result.map_or_else(String::new, |r| human_bytes(r.content.len()));
        let error_note = result
            .filter(|r| r.is_error)
            .map(|r| format!("  {}", truncate_inline(&r.content, 140).red()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}   {} {name:<14} [{short_id}]  {len:>8}{error_note}\n",
            "│".dimmed(),
            glyph.to_string().color(glyph_color),
            name = call.name,
            len = len_str,
        ));
        if let Some(summary) = tool_arg_summary(&call.name, &call.input) {
            out.push_str(&format!(
                "{}      {}\n",
                "│".dimmed(),
                truncate_inline(&summary, 160).bright_black(),
            ));
        }
    }
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Human-readable label for a [`StreamPhase`] used in the live
/// transcript trail.
fn stream_phase_label(phase: StreamPhase) -> &'static str {
    match phase {
        StreamPhase::Connecting => "stream open",
        StreamPhase::Thinking => "thinking",
        StreamPhase::Text => "writing",
        StreamPhase::ToolInput => "tool_input",
        StreamPhase::Ping => "ping",
    }
}

/// Emit a single continuation line marking a streaming phase
/// transition (e.g. the model entering its extended-thinking block),
/// stamped with the elapsed time since the request opened.
///
/// Rendered between the request block and the eventual response /
/// tools block so a long, otherwise-silent turn shows a live trail of
/// what the model is doing — and the `t+` clock makes a creeping
/// stall visible before the per-event liveness timeout fires.
pub fn stream_phase_line(phase: StreamPhase, elapsed: Duration) {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let body = format!(
        "{}   {} {:<12} (t+{})",
        "│".dimmed(),
        "⋯".bright_black(),
        stream_phase_label(phase),
        human_duration_ms(elapsed_ms),
    );
    info!(target: CONSOLE_TARGET, "{body}");
}

/// Inputs to [`stream_timeout_block`]. Captures the state of the
/// stalled stream at the moment the per-event liveness timeout fired.
pub struct StreamTimeoutView {
    /// Configured `stream_event_timeout`, in milliseconds.
    pub elapsed_ms: u64,
    /// Last streaming phase observed before the stall, if any frame
    /// arrived at all (`None` => the stream stalled before the first
    /// liveness frame, i.e. a slow time-to-first-byte).
    pub last_phase: Option<StreamPhase>,
    /// Bytes of thinking content accumulated before the stall.
    pub thinking_bytes: usize,
    /// Bytes of assistant text accumulated before the stall.
    pub text_bytes: usize,
}

/// Render a dedicated failure block when the streaming pump's
/// per-event liveness timeout fires. Symmetric to
/// [`anthropic_response_block`] but red — turns the previously-silent
/// 90s gap into an actionable box naming the stalled phase and how
/// much content had streamed before the stream went quiet.
pub fn stream_timeout_block(view: &StreamTimeoutView) {
    let mut out = String::new();
    let header = "← stream_event_timeout".red().bold();
    let tag = destination_tag("aura-network", None);
    out.push_str(&format!("{} {header}  {tag}\n", "┌─".dimmed()));
    out.push_str(&wrap_row(
        "class",
        &format!(
            "{:<22} elapsed {:>7}",
            "stream_event_timeout",
            human_duration_ms(view.elapsed_ms)
        ),
    ));
    let phase_label = view
        .last_phase
        .map_or("none (no content yet)", stream_phase_label);
    out.push_str(&wrap_row(
        "stalled",
        &format!(
            "last phase={phase_label} · {} thinking / {} text",
            human_bytes(view.thinking_bytes),
            human_bytes(view.text_bytes),
        ),
    ));
    out.push_str(&format!("{}", "└─".dimmed()));
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Banner emitted at the top of a freshly-minted task. Replaces the
/// pre-block `"Starting agent task"` info log; spans carry the rest.
pub fn task_start_banner(task_id: &str, max_turns: u32, max_iterations_per_task: u32) {
    let short = short_task_id(task_id);
    let line = format!(
        "═══ task {short} started · max_turns {max_turns} · max_iterations {max_iterations_per_task} ═══"
    );
    info!(target: CONSOLE_TARGET, "{}", line.bright_green().bold());
}

/// Inputs to [`anthropic_request_block`]. Passing a borrowed view
/// avoids a string-clone storm at the call site.
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
    /// Short semantic label for the network destination (e.g.
    /// `"aura-network"` for any call routed through the LLM proxy).
    pub destination: &'a str,
    /// Host portion of the target URL (e.g.
    /// `"aura-router.onrender.com"`). Surfaced alongside
    /// [`Self::destination`] in the request header.
    pub destination_host: &'a str,
}

/// Inputs to [`anthropic_response_block`].
pub struct AnthropicResponseView<'a> {
    pub status_code: u16,
    pub status_text: &'a str,
    pub stop_reason: &'a str,
    pub elapsed_ms: u64,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub request_id: Option<&'a str>,
    /// Short semantic label for the network destination (e.g.
    /// `"aura-network"`). Host is omitted on the response side to
    /// keep the header line scannable — the request header above
    /// already carries the full host.
    pub destination: &'a str,
}

// ----------------------------------------------------------------------
// Formatting primitives
// ----------------------------------------------------------------------

/// Render `[destination · host]` (host optional) styled as dim cyan
/// so it sits subtly next to the bold header text without fighting
/// the status-family color.
fn destination_tag(destination: &str, host: Option<&str>) -> String {
    let body = match host {
        Some(h) if !h.is_empty() => format!("[{destination} · {h}]"),
        _ => format!("[{destination}]"),
    };
    body.cyan().dimmed().to_string()
}

/// Render a labeled row, wrapping long values so continuation lines
/// stay indented under the value column (column [`ROW_INDENT_COLS`]).
///
/// When stdout is not a TTY (or [`terminal_size`] reports nothing),
/// this falls back to the original single-line behaviour so log files
/// stay byte-identical to the pre-wrap layout.
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

/// Detect terminal width in columns. Returns `None` when stdout is
/// not a TTY (e.g. piped to a file) so callers can skip wrapping.
fn detect_term_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize)
}

/// Split `value` into chunks of at most `max` chars, preferring to
/// break after the last comma or whitespace in the window so that
/// comma-separated lists (e.g. the `headers` row) wrap cleanly
/// between items. Falls back to a hard split when no preferred
/// break point is found.
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
        // Prefer last comma inside the window (split *after* the comma
        // so the punctuation stays attached to the preceding token).
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
        // Defensive: never make zero progress.
        let split_at = split_at.max(start + 1);
        let chunk: String = chars[start..split_at].iter().collect();
        chunks.push(chunk.trim_end().to_string());
        // Skip any whitespace at the start of the next chunk so the
        // continuation lines up cleanly under the value column.
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

/// Render the first 8 hex chars of a task UUID (the prefix is
/// universally enough to disambiguate concurrent tasks in a single
/// log file). Accepts any string; if it's too short, the original
/// is returned unchanged.
fn short_task_id(task_id: &str) -> &str {
    task_id.get(..8).unwrap_or(task_id)
}

/// Render `toolu_…AbCd` from `toolu_01PEmY2JQVEmZh8FErm4R56d`. Keeps
/// the recognizable Anthropic prefix and the last 4 chars so two
/// concurrent calls in the same batch don't collide visually.
fn short_tool_use_id(id: &str) -> String {
    if id.len() <= 12 {
        return id.to_string();
    }
    let last4: String = id
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let prefix = id.get(..6).unwrap_or(id);
    format!("{prefix}…{last4}")
}

/// Extract the primary argument for a tool call so the console can
/// show *what* a call operated on (the command, path, pattern, …) on
/// its own line beneath the tool name. Returns `None` for tools with
/// no single meaningful scalar argument.
fn tool_arg_summary(name: &str, input: &serde_json::Value) -> Option<String> {
    let str_field = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::to_string);
    match name {
        "read_file" | "write_file" | "edit_file" | "delete_file" | "list_files" | "stat_file" => {
            str_field("path")
        }
        "search_code" | "find_files" => str_field("pattern"),
        "git_commit" | "git_commit_push" => str_field("message"),
        "git_push" => str_field("branch").or_else(|| str_field("remote_url")),
        "run_command" => run_command_summary(input),
        _ => None,
    }
}

/// Reconstruct the command line for a `run_command` call, mirroring
/// `parse_run_args` in `aura-tools`: prefer `program` + `args`, then
/// fall back to `shell_script`, then the legacy `command` alias.
fn run_command_summary(input: &serde_json::Value) -> Option<String> {
    if let Some(program) = input.get("program").and_then(|v| v.as_str()) {
        let mut parts = vec![program.to_string()];
        if let Some(args) = input.get("args").and_then(|v| v.as_array()) {
            parts.extend(args.iter().filter_map(|a| a.as_str().map(str::to_string)));
        }
        return Some(parts.join(" "));
    }
    input
        .get("shell_script")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("command").and_then(|v| v.as_str()))
        .map(str::to_string)
}

/// Collapse control chars / whitespace / quotes into a single inline
/// preview for error messages embedded in a block row.
fn truncate_inline(content: &str, limit: usize) -> String {
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

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn short_task_id_takes_first_eight() {
        assert_eq!(short_task_id("9170539f1ae70779"), "9170539f");
        assert_eq!(short_task_id("short"), "short");
    }

    #[test]
    fn short_tool_use_id_keeps_prefix_and_suffix() {
        assert_eq!(
            short_tool_use_id("toolu_01PEmY2JQVEmZh8FErm4R56d"),
            "toolu_…R56d"
        );
        assert_eq!(short_tool_use_id("toolu_short"), "toolu_short");
    }

    #[test]
    fn truncate_inline_collapses_and_clips() {
        let s = "hello\n  world  \"quoted\" extra extra extra extra extra extra";
        let out = truncate_inline(s, 30);
        assert!(out.starts_with("hello world 'quoted'"));
        assert!(out.ends_with('…') || out.len() <= 30);
    }

    #[test]
    fn wrap_value_breaks_on_comma_preference() {
        // Width 30 forces a wrap; expect breaks AFTER commas, not
        // mid-token.
        let chunks = wrap_value_chunks("aaa,bbb,ccc,ddd,eee,fff,ggg", 12);
        // Every non-final chunk should end with a comma (proves the
        // preferred break point was used).
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(
                chunk.ends_with(','),
                "expected comma-terminated chunk, got {chunk:?}"
            );
        }
        assert_eq!(chunks.join(""), "aaa,bbb,ccc,ddd,eee,fff,ggg");
    }

    #[test]
    fn wrap_value_breaks_on_whitespace_when_no_comma() {
        let chunks = wrap_value_chunks("alpha beta gamma delta epsilon zeta", 12);
        assert!(chunks.len() > 1, "expected wrap, got {chunks:?}");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 12, "chunk over budget: {chunk:?}");
        }
        // Leading whitespace is consumed at chunk boundaries.
        for chunk in &chunks {
            assert!(
                !chunk.starts_with(' '),
                "leading whitespace not trimmed: {chunk:?}"
            );
        }
    }

    #[test]
    fn wrap_value_hard_splits_when_no_break() {
        let chunks = wrap_value_chunks("aaaaaaaaaaaaaaaaa", 5);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 5);
        }
        assert_eq!(chunks.concat(), "aaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn wrap_row_no_tty_returns_single_line() {
        // In `cargo test` stdout is captured (not a TTY), so
        // `detect_term_width` returns None and we get the legacy
        // single-line layout — with colors disabled so the assertion
        // can match plain bytes.
        colored::control::set_override(false);
        let out = wrap_row("model", "claude-opus-4-6");
        assert_eq!(out, "│   model          claude-opus-4-6\n");
        colored::control::unset_override();
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
    fn stream_phase_line_renders_each_phase() {
        for phase in [
            StreamPhase::Connecting,
            StreamPhase::Thinking,
            StreamPhase::Text,
            StreamPhase::ToolInput,
            StreamPhase::Ping,
        ] {
            stream_phase_line(phase, Duration::from_millis(2_300));
        }
    }

    #[test]
    fn stream_timeout_block_renders_with_and_without_phase() {
        stream_timeout_block(&StreamTimeoutView {
            elapsed_ms: 90_000,
            last_phase: Some(StreamPhase::Thinking),
            thinking_bytes: 7_480,
            text_bytes: 0,
        });
        stream_timeout_block(&StreamTimeoutView {
            elapsed_ms: 90_000,
            last_phase: None,
            thinking_bytes: 0,
            text_bytes: 0,
        });
    }

    #[test]
    fn run_command_summary_joins_program_and_args() {
        let input = serde_json::json!({
            "program": "cargo",
            "args": ["build", "--release"]
        });
        assert_eq!(
            run_command_summary(&input),
            Some("cargo build --release".to_string())
        );
    }

    #[test]
    fn run_command_summary_falls_back_to_shell_and_legacy_command() {
        let shell = serde_json::json!({ "shell_script": "echo hi && ls" });
        assert_eq!(
            run_command_summary(&shell),
            Some("echo hi && ls".to_string())
        );
        let legacy = serde_json::json!({ "command": "make test" });
        assert_eq!(run_command_summary(&legacy), Some("make test".to_string()));
    }

    #[test]
    fn tool_arg_summary_maps_each_tool_to_its_primary_field() {
        assert_eq!(
            tool_arg_summary("read_file", &serde_json::json!({ "path": "src/lib.rs" })),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(
            tool_arg_summary("search_code", &serde_json::json!({ "pattern": "fn main" })),
            Some("fn main".to_string())
        );
        assert_eq!(
            tool_arg_summary("git_commit", &serde_json::json!({ "message": "fix bug" })),
            Some("fix bug".to_string())
        );
        assert_eq!(
            tool_arg_summary("get_task_context", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn tools_block_renders_status_glyphs() {
        let calls = vec![
            ToolCallInfo {
                id: "toolu_aaaaaaaa1234".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            },
            ToolCallInfo {
                id: "toolu_bbbbbbbb5678".into(),
                name: "list_files".into(),
                input: serde_json::json!({}),
            },
        ];
        let results = vec![
            ToolCallResult {
                tool_use_id: "toolu_aaaaaaaa1234".into(),
                content: "ok".into(),
                is_error: false,
                kind: aura_core_types::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
                image: None,
            },
            ToolCallResult {
                tool_use_id: "toolu_bbbbbbbb5678".into(),
                content: "boom".into(),
                is_error: true,
                kind: aura_core_types::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
                image: None,
            },
        ];
        let cached: HashSet<String> = HashSet::new();
        let blocked: HashSet<String> = HashSet::new();
        // Smoke-test: tools_block emits via `tracing::info!`; just
        // exercise the rendering path.
        tools_block(&calls, &results, &cached, &blocked);
    }
}
