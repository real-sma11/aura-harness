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
//! The block helpers are pure string builders; they do not touch any
//! shared state. They are kept here (rather than inside individual
//! callers) so the rendering rules stay in one place and snapshot
//! tests can pin the exact byte layout.

use tracing::info;

use crate::types::{ToolCallInfo, ToolCallResult};

/// Tracing target the custom formatter recognises and renders verbatim.
pub const CONSOLE_TARGET: &str = "aura::console";

/// Render `─── task <8> turn N sampling I ───` as a separator at the
/// top of every sampling request. Emitted by
/// [`crate::agent_loop::sampling::run_sampling_request`].
pub fn sampling_boundary(task_id: &str, turn: u32, iter: usize) {
    let short = short_task_id(task_id);
    let line = format!(
        "─── task {short} · turn {turn} · sampling {iter} ──────────────────────────────"
    );
    info!(target: CONSOLE_TARGET, "{line}");
}

/// Render the request half of an Anthropic call as a multi-line block.
/// Field arguments are passed verbatim and trusted; the helper does
/// not redact or truncate.
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
    out.push_str(&format!(
        "┌─ tools  ({} requested · {} cached · {} to execute)\n",
        calls.len(),
        cached_ids.len(),
        executed,
    ));

    for call in calls {
        let result = results.iter().find(|r| r.tool_use_id == call.id);
        let glyph = match result {
            None => '·',
            Some(r) if r.is_error => '✗',
            Some(_) if cached_ids.contains(&call.id) => '↺',
            Some(_) if blocked_ids.contains(&call.id) => '⊘',
            Some(_) => '✓',
        };
        let short_id = short_tool_use_id(&call.id);
        let len_str = result.map_or_else(String::new, |r| human_bytes(r.content.len()));
        let error_note = result
            .filter(|r| r.is_error)
            .map(|r| format!("  {}", truncate_inline(&r.content, 140)))
            .unwrap_or_default();
        out.push_str(&format!(
            "│   {glyph} {name:<14} [{short_id}]  {len:>8}{error_note}\n",
            name = call.name,
            len = len_str,
        ));
    }
    out.push_str("└─");
    info!(target: CONSOLE_TARGET, "{out}");
}

/// Banner emitted at the top of a freshly-minted task. Replaces the
/// pre-block `"Starting agent task"` info log; spans carry the rest.
pub fn task_start_banner(task_id: &str, max_turns: u32, max_iterations_per_task: u32) {
    let short = short_task_id(task_id);
    let line = format!(
        "═══ task {short} started · max_turns {max_turns} · max_iterations {max_iterations_per_task} ═══"
    );
    info!(target: CONSOLE_TARGET, "{line}");
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
    let last4: String = id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix = id.get(..6).unwrap_or(id);
    format!("{prefix}…{last4}")
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
                kind: aura_core::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
            },
            ToolCallResult {
                tool_use_id: "toolu_bbbbbbbb5678".into(),
                content: "boom".into(),
                is_error: true,
                kind: aura_core::ToolResultKind::Ok,
                stop_loop: false,
                file_changes: Vec::new(),
            },
        ];
        let cached: HashSet<String> = HashSet::new();
        let blocked: HashSet<String> = HashSet::new();
        // Smoke-test: tools_block emits via `tracing::info!`; just
        // exercise the rendering path.
        tools_block(&calls, &results, &cached, &blocked);
    }
}
