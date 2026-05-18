//! Message-history compaction and summarization helpers.

use aura_reasoner::{ContentBlock, Message, ToolResultContent};
use serde_json::Value;
use tracing::{debug, info};

const CHARS_PER_TOKEN: usize = 4;
const COMPACTION_TIER_HISTORY: f64 = 0.85;
const COMPACTION_TIER_AGGRESSIVE: f64 = 0.70;
const COMPACTION_TIER_60: f64 = 0.60;
const COMPACTION_TIER_30: f64 = 0.30;
const COMPACTION_TIER_MICRO: f64 = 0.15;

/// Absolute message-byte threshold for light compaction.
pub const ABSOLUTE_BYTE_LIGHT_AT: usize = 64 * 1024;
/// Absolute message-byte threshold for aggressive compaction.
pub const ABSOLUTE_BYTE_AGGRESSIVE_AT: usize = 96 * 1024;
/// Absolute message-byte threshold for micro compaction.
pub const ABSOLUTE_BYTE_MICRO_AT: usize = 128 * 1024;

/// Hard upper bound on bytes-per-tool-blob kept in session storage.
pub const SESSION_TOOL_BLOB_MAX_BYTES: usize = 8 * 1024;

/// Compaction tier configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Maximum characters for tool results in older messages.
    pub tool_result_max_chars: usize,
    /// Maximum characters for plain text in older messages.
    pub text_max_chars: usize,
    /// Number of recent messages to preserve uncompacted.
    pub preserve_recent: usize,
}

impl CompactionConfig {
    /// Micro tier: very aggressive truncation for near-limit contexts (>=85%).
    #[must_use]
    pub const fn micro() -> Self {
        Self {
            tool_result_max_chars: 200,
            text_max_chars: 400,
            preserve_recent: 2,
        }
    }

    /// Aggressive tier: significant truncation for high-utilization contexts (>=70%).
    #[must_use]
    pub const fn aggressive() -> Self {
        Self {
            tool_result_max_chars: 500,
            text_max_chars: 800,
            preserve_recent: 4,
        }
    }

    /// Moderate tier: balanced truncation at medium-high utilization (>=60%).
    #[must_use]
    pub const fn moderate() -> Self {
        Self {
            tool_result_max_chars: 1000,
            text_max_chars: 1500,
            preserve_recent: 6,
        }
    }

    /// Light tier: gentle truncation for moderate utilization (>=30%).
    #[must_use]
    pub const fn light() -> Self {
        Self {
            tool_result_max_chars: 3000,
            text_max_chars: 4000,
            preserve_recent: 8,
        }
    }

    /// History tier: minimal truncation preserving most context (>=15%).
    #[must_use]
    pub const fn history() -> Self {
        Self {
            tool_result_max_chars: 1500,
            text_max_chars: 2000,
            preserve_recent: 6,
        }
    }
}

/// Request policy used when choosing a message-compaction tier.
#[derive(Debug, Clone, Copy)]
pub struct CompactionPolicy {
    /// Model context window in tokens. `None` disables utilization-based selection.
    pub max_context_tokens: Option<u64>,
    /// Latest context estimate in tokens before output-reserve pressure is applied.
    pub estimated_context_tokens: u64,
    /// Response token reserve included in pressure calculations.
    pub reserved_output_tokens: u64,
}

impl CompactionPolicy {
    /// Build the policy used by the agent loop from existing token estimates.
    #[must_use]
    pub const fn new(
        max_context_tokens: Option<u64>,
        estimated_context_tokens: u64,
        reserved_output_tokens: u64,
    ) -> Self {
        Self {
            max_context_tokens,
            estimated_context_tokens,
            reserved_output_tokens,
        }
    }
}

/// Mutable input bundle for message compaction.
pub struct CompactionInput<'a> {
    /// Messages to compact in place.
    pub messages: &'a mut [Message],
    /// Tier-selection policy.
    pub policy: CompactionPolicy,
}

/// Report returned by message compaction operations.
#[derive(Debug, Clone, Copy)]
pub struct CompactionReport {
    /// Character estimate before compaction.
    pub before_chars: usize,
    /// Character estimate after compaction.
    pub after_chars: usize,
    /// Action selected for this compaction pass.
    pub action: CompactionAction,
}

impl CompactionReport {
    /// Whether the operation reduced the estimated message footprint.
    #[must_use]
    pub const fn reduced(&self) -> bool {
        self.after_chars < self.before_chars
    }
}

/// Message-compaction action selected for a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionAction {
    /// No compaction tier was selected.
    None,
    /// A tier was selected and applied.
    Applied(CompactionConfig),
}

/// Compatibility marker names used by Phase 1 redaction summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionMarker {
    /// `write_file.content` was summarized.
    WriteContent,
    /// `edit_file.old_*` was summarized.
    EditOld,
    /// `edit_file.new_*` was summarized.
    EditNew,
    /// A stored tool blob exceeded the storage cap.
    StorageBlob,
}

/// Overflow recovery step used by the existing retry sequence.
#[derive(Debug, Clone, Copy)]
pub struct OverflowStep {
    /// Tier to apply before retrying.
    pub tier: CompactionConfig,
    /// User-facing warning emitted by the caller.
    pub warning: &'static str,
}

impl OverflowStep {
    /// Existing first recovery step.
    pub const AGGRESSIVE: Self = Self {
        tier: CompactionConfig::aggressive(),
        warning:
            "Context limit reached; compacting older context, trimming response budget, and retrying.",
    };

    /// Existing emergency recovery step.
    pub const MICRO: Self = Self {
        tier: CompactionConfig::micro(),
        warning:
            "Context is still too large; applying emergency compaction, trimming response budget again, and retrying.",
    };
}

/// Input to a write-input or cached-result summary operation.
#[derive(Debug, Clone, Copy)]
pub struct SummaryInput<'a> {
    /// Tool name associated with the payload.
    pub tool_name: &'a str,
    /// Tool JSON input.
    pub input: &'a Value,
    /// Optional tool-result text when summarizing cached reads.
    pub content: Option<&'a str>,
}

/// Output of a summary operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutput {
    /// Replacement JSON input for a tool-use block.
    Input(Value),
    /// Replacement text for a cached tool result.
    Text(String),
}

/// Truncate a string to the given max chars, preserving head and tail.
///
/// `head_chars` and `tail_chars` control how many characters to keep from
/// the beginning and end respectively. Pass `None` to use 1/3 of `max_chars`.
#[must_use]
pub fn truncate_content(
    content: &str,
    max_chars: usize,
    head_chars: Option<usize>,
    tail_chars: Option<usize>,
) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }

    let mut head = head_chars.unwrap_or(max_chars / 3);
    let mut tail = tail_chars.unwrap_or(max_chars / 3);

    if head + tail > max_chars {
        let requested_total = head + tail;
        if requested_total == 0 {
            head = 0;
            tail = 0;
        } else {
            head = max_chars.saturating_mul(head) / requested_total;
            tail = max_chars.saturating_sub(head);
        }
    }

    let head = head.min(content.len());
    let tail = tail.min(content.len().saturating_sub(head));

    let head_part: String = content.chars().take(head).collect();
    let tail_part: String = content
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let omitted = content.len().saturating_sub(head + tail);
    format!("{head_part}\n\n[...content truncated ({omitted} chars omitted)...]\n\n{tail_part}")
}

/// Estimate total character count of messages.
#[must_use]
pub fn estimate_message_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::Thinking { thinking, .. } => thinking.len(),
                    ContentBlock::ToolUse { input, .. } => {
                        serde_json::to_string(input).map_or(0, |s| s.len())
                    }
                    ContentBlock::ToolResult { content, .. } => match content {
                        ToolResultContent::Text(t) => t.len(),
                        ToolResultContent::Json(v) => {
                            serde_json::to_string(v).map_or(0, |s| s.len())
                        }
                    },
                    ContentBlock::Image { source } => source.data.len(),
                })
                .sum::<usize>()
        })
        .sum()
}

/// Select the compaction tier based on context utilization percentage.
///
/// Higher utilization means more aggressive compaction. Returns `None` below 15%.
#[must_use]
pub fn select_tier(utilization: f64) -> Option<CompactionConfig> {
    if utilization >= COMPACTION_TIER_HISTORY {
        Some(CompactionConfig::micro())
    } else if utilization >= COMPACTION_TIER_AGGRESSIVE {
        Some(CompactionConfig::aggressive())
    } else if utilization >= COMPACTION_TIER_60 {
        Some(CompactionConfig::moderate())
    } else if utilization >= COMPACTION_TIER_30 {
        Some(CompactionConfig::light())
    } else if utilization >= COMPACTION_TIER_MICRO {
        Some(CompactionConfig::history())
    } else {
        None
    }
}

/// Tiered selector keyed off raw message-character bytes.
#[must_use]
pub fn absolute_byte_tier(messages_chars: usize) -> Option<CompactionConfig> {
    if messages_chars >= ABSOLUTE_BYTE_MICRO_AT {
        Some(CompactionConfig::micro())
    } else if messages_chars >= ABSOLUTE_BYTE_AGGRESSIVE_AT {
        Some(CompactionConfig::aggressive())
    } else if messages_chars >= ABSOLUTE_BYTE_LIGHT_AT {
        Some(CompactionConfig::light())
    } else {
        None
    }
}

/// Pick whichever tier trims more aggressively.
#[must_use]
pub fn pick_stricter_tier(
    a: Option<CompactionConfig>,
    b: Option<CompactionConfig>,
) -> Option<CompactionConfig> {
    match (a, b) {
        (Some(x), Some(y)) => {
            if y.tool_result_max_chars < x.tool_result_max_chars {
                Some(y)
            } else {
                Some(x)
            }
        }
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// Best-effort Rust signature extraction.
///
/// If `content` looks like Rust code, replaces function/method bodies with
/// a placeholder, keeping signatures and structural items visible.
#[must_use]
pub fn try_signature_compact(content: &str) -> Option<String> {
    const RUST_MARKERS: &[&str] = &["fn ", "pub fn", "struct ", "impl ", "mod "];
    let has_rust = RUST_MARKERS.iter().any(|m| content.contains(m));
    if !has_rust {
        return None;
    }

    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    let mut line_buf = String::new();
    let mut brace_depth: i32 = 0;
    let mut in_body = false;
    let mut body_start_depth: i32 = 0;
    let mut wrote_placeholder = false;

    while let Some(ch) = chars.next() {
        if ch == '\n' || chars.peek().is_none() {
            if ch != '\n' {
                line_buf.push(ch);
            }

            let trimmed = line_buf.trim_start();
            let is_sig_line = trimmed.starts_with("pub fn ")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("pub async fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("pub unsafe fn ")
                || trimmed.starts_with("unsafe fn ")
                || trimmed.starts_with("pub const fn ")
                || trimmed.starts_with("const fn ");

            if in_body {
                for c in line_buf.chars() {
                    match c {
                        '{' => brace_depth += 1,
                        '}' => brace_depth -= 1,
                        _ => {}
                    }
                }

                if brace_depth <= body_start_depth {
                    if !wrote_placeholder {
                        result.push_str("    // ... body omitted ...\n");
                    }
                    result.push_str(&line_buf);
                    result.push('\n');
                    in_body = false;
                } else if !wrote_placeholder {
                    result.push_str("    // ... body omitted ...\n");
                    wrote_placeholder = true;
                }
            } else if is_sig_line && line_buf.contains('{') {
                result.push_str(&line_buf);
                result.push('\n');

                let open_count = line_buf
                    .chars()
                    .filter(|&c| c == '{')
                    .fold(0_i32, |count, _| count.saturating_add(1));
                let close_count = line_buf
                    .chars()
                    .filter(|&c| c == '}')
                    .fold(0_i32, |count, _| count.saturating_add(1));
                brace_depth += open_count - close_count;

                if brace_depth > 0 {
                    in_body = true;
                    body_start_depth = brace_depth - 1;
                    wrote_placeholder = false;
                }
            } else {
                for c in line_buf.chars() {
                    match c {
                        '{' => brace_depth += 1,
                        '}' => brace_depth -= 1,
                        _ => {}
                    }
                }
                result.push_str(&line_buf);
                result.push('\n');
            }

            line_buf.clear();
        } else {
            line_buf.push(ch);
        }
    }

    if result.len().saturating_mul(10) <= content.len().saturating_mul(7) {
        Some(result)
    } else {
        None
    }
}

struct CompactionParams {
    compact_end: usize,
    head_chars: Option<usize>,
    tail_chars: Option<usize>,
}

const fn select_compaction_candidates(
    messages: &[Message],
    config: &CompactionConfig,
) -> Option<CompactionParams> {
    if messages.len() <= config.preserve_recent + 1 {
        return None;
    }
    let compact_end = messages.len().saturating_sub(config.preserve_recent);
    let is_micro = config.preserve_recent == CompactionConfig::micro().preserve_recent
        && config.tool_result_max_chars == CompactionConfig::micro().tool_result_max_chars;
    let (head_chars, tail_chars) = if is_micro {
        (Some(6000_usize), Some(3000_usize))
    } else {
        (None, None)
    };
    Some(CompactionParams {
        compact_end,
        head_chars,
        tail_chars,
    })
}

fn compact_content_block(
    block: &mut ContentBlock,
    config: &CompactionConfig,
    head_chars: Option<usize>,
    tail_chars: Option<usize>,
) {
    match block {
        ContentBlock::ToolResult { content, .. } => {
            let text = match content {
                ToolResultContent::Text(t) => t.clone(),
                ToolResultContent::Json(v) => serde_json::to_string(v).unwrap_or_default(),
            };
            if text.len() > config.tool_result_max_chars {
                let compacted = try_signature_compact(&text).unwrap_or_else(|| {
                    truncate_content(&text, config.tool_result_max_chars, head_chars, tail_chars)
                });
                if compacted.len() <= config.tool_result_max_chars || compacted.len() < text.len() {
                    *content = ToolResultContent::Text(compacted);
                } else {
                    *content = ToolResultContent::Text(truncate_content(
                        &text,
                        config.tool_result_max_chars,
                        head_chars,
                        tail_chars,
                    ));
                }
            }
        }
        ContentBlock::Text { text } => {
            if text.len() > config.text_max_chars {
                if let Some(sig) = try_signature_compact(text) {
                    if sig.len() <= config.text_max_chars || sig.len() < text.len() {
                        *text = sig;
                    } else {
                        *text =
                            truncate_content(text, config.text_max_chars, head_chars, tail_chars);
                    }
                } else {
                    *text = truncate_content(text, config.text_max_chars, head_chars, tail_chars);
                }
            }
        }
        _ => {}
    }
}

/// Compact older messages using the given tier configuration.
pub fn compact_older_messages(messages: &mut [Message], config: &CompactionConfig) {
    let Some(params) = select_compaction_candidates(messages, config) else {
        return;
    };
    for msg in &mut messages[1..params.compact_end] {
        for block in &mut msg.content {
            compact_content_block(block, config, params.head_chars, params.tail_chars);
        }
    }
}

/// Choose and apply a compaction tier using utilization and absolute-byte guards.
#[allow(clippy::needless_pass_by_value)]
pub fn compact_messages(input: CompactionInput<'_>) -> CompactionReport {
    let before_chars = estimate_message_chars(input.messages);
    let utilization_tier = input.policy.max_context_tokens.and_then(|max_ctx| {
        let pressure_tokens = input
            .policy
            .estimated_context_tokens
            .saturating_add(input.policy.reserved_output_tokens)
            .min(max_ctx);
        #[allow(clippy::cast_precision_loss)]
        let utilization = pressure_tokens as f64 / max_ctx as f64;
        select_tier(utilization)
    });
    let absolute_tier = absolute_byte_tier(before_chars);
    let chosen = pick_stricter_tier(utilization_tier, absolute_tier);

    if let Some(tier) = chosen {
        debug!("Compacting context");
        let absolute_won = match (utilization_tier, absolute_tier) {
            (None, Some(_)) => true,
            (Some(u), Some(a)) => a.tool_result_max_chars < u.tool_result_max_chars,
            _ => false,
        };
        if absolute_won {
            info!(
                messages_chars = before_chars,
                tool_result_max_chars = tier.tool_result_max_chars,
                text_max_chars = tier.text_max_chars,
                preserve_recent = tier.preserve_recent,
                "compaction triggered by absolute_bytes (proxy-envelope guard)",
            );
        }
        compact_older_messages(input.messages, &tier);
    }

    let after_chars = estimate_message_chars(input.messages);
    CompactionReport {
        before_chars,
        after_chars,
        action: chosen.map_or(CompactionAction::None, CompactionAction::Applied),
    }
}

/// Apply a specific overflow-recovery tier to messages.
pub fn recover_overflow(messages: &mut [Message], tier: CompactionConfig) -> CompactionReport {
    let before_chars = estimate_message_chars(messages);
    compact_older_messages(messages, &tier);
    let after_chars = estimate_message_chars(messages);
    CompactionReport {
        before_chars,
        after_chars,
        action: CompactionAction::Applied(tier),
    }
}

/// Apply proactive exploration compaction when the caller has crossed its threshold.
pub fn compact_exploration_if_needed(
    messages: &mut [Message],
    exploration_count: usize,
    exploration_allowance: usize,
    max_context_tokens: Option<u64>,
    already_compacted: bool,
) -> bool {
    if already_compacted || max_context_tokens.is_none() {
        return false;
    }
    let threshold = (exploration_allowance * 2) / 3;
    if exploration_count < threshold {
        return false;
    }

    let tier = CompactionConfig::history();
    compact_older_messages(messages, &tier);
    debug!(
        exploration_count,
        threshold, "Proactive compaction triggered by exploration usage"
    );
    true
}

/// Convert message characters to the existing heuristic token estimate.
#[must_use]
pub fn message_chars_to_tokens(chars: usize) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (chars / CHARS_PER_TOKEN) as u64
    }
}

/// Summarize write tool inputs to save context tokens.
#[must_use]
pub fn summarize_write_input(tool_name: &str, input: &Value) -> Option<Value> {
    match tool_name {
        "write_file" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content_len = input
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            Some(serde_json::json!({
                "path": path,
                "content": format!("<<<AURA_ELIDED_CONTENT::{content_len}_bytes>>>")
            }))
        }
        "edit_file" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let old_key = if input.get("old_string").is_some() {
                "old_string"
            } else {
                "old_text"
            };
            let new_key = if input.get("new_string").is_some() {
                "new_string"
            } else {
                "new_text"
            };
            let old_len = input
                .get(old_key)
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            let new_len = input
                .get(new_key)
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            let mut summarized = serde_json::Map::new();
            summarized.insert("path".to_string(), serde_json::json!(path));
            summarized.insert(
                old_key.to_string(),
                serde_json::json!(format!("<<<AURA_ELIDED_OLD::{old_len}_chars>>>")),
            );
            summarized.insert(
                new_key.to_string(),
                serde_json::json!(format!("<<<AURA_ELIDED_NEW::{new_len}_chars>>>")),
            );
            Some(Value::Object(summarized))
        }
        _ => None,
    }
}

/// Collapse oversized repeated cache hits for read-only tools.
#[must_use]
pub fn summarize_cached_tool_result(
    tool_name: &str,
    input: &Value,
    content: &str,
) -> Option<String> {
    if std::env::var("AURA_DISABLE_CACHED_RESULT_SHAPING")
        .ok()
        .as_deref()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    {
        return None;
    }

    let (reuse_threshold, max_chars, head_chars, tail_chars) = match tool_name {
        "read_file" => (8_000, 4_000, 3_000, 500),
        "search_code" => (4_000, 2_000, 1_500, 250),
        "list_files" | "find_files" => (2_500, 1_200, 900, 150),
        "stat_file" => (1_500, 900, 650, 100),
        _ => return None,
    };

    if content.len() <= reuse_threshold {
        return None;
    }

    let descriptor = cached_tool_descriptor(input);
    let truncated = truncate_content(content, max_chars, Some(head_chars), Some(tail_chars));
    Some(format!(
        "Cached result reused from earlier identical `{tool_name}` call{descriptor}. Full output was {} chars.\n\n{truncated}",
        content.len()
    ))
}

/// Apply a write-input or cached-result summary.
#[must_use]
pub fn apply_summary(input: SummaryInput<'_>) -> Option<SummaryOutput> {
    if let Some(content) = input.content {
        summarize_cached_tool_result(input.tool_name, input.input, content).map(SummaryOutput::Text)
    } else {
        summarize_write_input(input.tool_name, input.input).map(SummaryOutput::Input)
    }
}

fn cached_tool_descriptor(input: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        parts.push(format!("path={path}"));
    }
    if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
        parts.push(format!("pattern={pattern}"));
    }
    if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
        parts.push(format!("query={query}"));
    }
    if let Some(start_line) = input.get("start_line").and_then(|v| v.as_u64()) {
        parts.push(format!("start_line={start_line}"));
    }
    if let Some(end_line) = input.get("end_line").and_then(|v| v.as_u64()) {
        parts.push(format!("end_line={end_line}"));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// Cap each `ToolUse` input / `ToolResult` content in `messages` at the storage limit.
pub fn truncate_messages_for_storage(messages: &mut [Message]) {
    fn truncate_str(s: &str, max: usize) -> Option<String> {
        if s.len() <= max {
            return None;
        }
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        Some(format!("{}... [truncated {} bytes]", &s[..end], s.len()))
    }

    for msg in messages.iter_mut() {
        for block in msg.content.iter_mut() {
            match block {
                ContentBlock::ToolUse { input, .. } => {
                    if let Ok(serialized) = serde_json::to_string(input) {
                        if let Some(truncated) =
                            truncate_str(&serialized, SESSION_TOOL_BLOB_MAX_BYTES)
                        {
                            *input = Value::String(truncated);
                        }
                    }
                }
                ContentBlock::ToolResult { content, .. } => match content {
                    ToolResultContent::Text(t) => {
                        if let Some(truncated) = truncate_str(t, SESSION_TOOL_BLOB_MAX_BYTES) {
                            *t = truncated;
                        }
                    }
                    ToolResultContent::Json(v) => {
                        if let Ok(serialized) = serde_json::to_string(v) {
                            if let Some(truncated) =
                                truncate_str(&serialized, SESSION_TOOL_BLOB_MAX_BYTES)
                            {
                                *content = ToolResultContent::Text(truncated);
                            }
                        }
                    }
                },
                _ => {}
            }
        }
    }
}

/// Public storage compaction API used by runtime.
pub fn compact_for_storage(messages: &mut [Message]) {
    truncate_messages_for_storage(messages);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_reasoner::Role;

    #[test]
    fn test_truncate_below_threshold() {
        let content = "short";
        assert_eq!(truncate_content(content, 100, None, None), "short");
    }

    #[test]
    fn test_truncate_preserves_head_and_tail() {
        let content = "a".repeat(300);
        let result = truncate_content(&content, 200, None, None);
        assert!(result.contains("content truncated"));
        assert!(result.len() < 300);
    }

    #[test]
    fn test_compact_older_preserves_recent() {
        let mut messages = vec![
            Message::user("first"),
            Message::user("second"),
            Message::user("third"),
            Message::user("fourth"),
        ];
        let config = CompactionConfig {
            tool_result_max_chars: 10,
            text_max_chars: 10,
            preserve_recent: 2,
        };
        compact_older_messages(&mut messages, &config);
        assert_eq!(messages.len(), 4);
    }

    #[test]
    fn test_select_tier_85pct() {
        let tier = select_tier(0.85);
        assert!(tier.is_some());
        let config = tier.unwrap();
        assert_eq!(
            config.preserve_recent,
            CompactionConfig::micro().preserve_recent
        );
        assert_eq!(
            config.tool_result_max_chars,
            CompactionConfig::micro().tool_result_max_chars
        );
    }

    #[test]
    fn test_select_tier_below_threshold() {
        let tier = select_tier(0.10);
        assert!(tier.is_none());
    }

    #[test]
    fn test_signature_extract_rust() {
        let rust_code = r#"
use std::io;

pub fn compute_sum(a: i32, b: i32) -> i32 {
let result = a + b;
println!("sum = {}", result);
if result > 100 {
    panic!("too big");
}
result
}

pub struct Config {
pub name: String,
pub value: u64,
}

impl Config {
pub fn new(name: &str) -> Self {
    Self {
        name: name.to_string(),
        value: 0,
    }
}

pub fn set_value(&mut self, v: u64) {
    self.value = v;
    println!("value set to {}", v);
    if v > 1000 {
        panic!("value too large");
    }
}
}

fn helper_internal() {
let x = 42;
let y = x * 2;
println!("{}", y);
for i in 0..10 {
    println!("{}", i);
}
}
"#;
        let result = try_signature_compact(rust_code);
        assert!(result.is_some(), "should extract Rust signatures");
        let extracted = result.unwrap();
        assert!(extracted.contains("pub fn compute_sum"));
        assert!(extracted.contains("// ... body omitted ..."));
        assert!(extracted.contains("pub fn new"));
        assert!(extracted.len() < rust_code.len());
    }

    #[test]
    fn test_signature_extract_non_rust() {
        let json = r#"{"key": "value", "nested": {"a": 1, "b": 2}}"#;
        assert!(try_signature_compact(json).is_none());

        let plain = "This is just some plain text with no code at all.\nIt has multiple lines.\nBut nothing resembling Rust.";
        assert!(try_signature_compact(plain).is_none());
    }

    #[test]
    fn test_5_tier_selection() {
        let t = select_tier(0.90).unwrap();
        assert_eq!(t.preserve_recent, 2);
        assert_eq!(t.tool_result_max_chars, 200);

        let t = select_tier(0.85).unwrap();
        assert_eq!(t.preserve_recent, 2);

        let t = select_tier(0.75).unwrap();
        assert_eq!(t.preserve_recent, 4);
        assert_eq!(t.tool_result_max_chars, 500);

        let t = select_tier(0.70).unwrap();
        assert_eq!(t.preserve_recent, 4);

        let t = select_tier(0.65).unwrap();
        assert_eq!(t.preserve_recent, 6);
        assert_eq!(t.tool_result_max_chars, 1000);
        assert_eq!(t.text_max_chars, 1500);

        let t = select_tier(0.60).unwrap();
        assert_eq!(t.preserve_recent, 6);

        let t = select_tier(0.45).unwrap();
        assert_eq!(t.preserve_recent, 8);
        assert_eq!(t.tool_result_max_chars, 3000);
        assert_eq!(t.text_max_chars, 4000);

        let t = select_tier(0.30).unwrap();
        assert_eq!(t.preserve_recent, 8);

        let t = select_tier(0.20).unwrap();
        assert_eq!(t.preserve_recent, 6);
        assert_eq!(t.tool_result_max_chars, 1500);
        assert_eq!(t.text_max_chars, 2000);

        let t = select_tier(0.15).unwrap();
        assert_eq!(t.preserve_recent, 6);

        assert!(select_tier(0.10).is_none());
        assert!(select_tier(0.0).is_none());
    }

    #[test]
    fn test_configurable_head_tail() {
        let content = "a".repeat(10_000);

        let result_default = truncate_content(&content, 3000, None, None);
        assert!(result_default.starts_with(&"a".repeat(1000)));
        assert!(result_default.ends_with(&"a".repeat(1000)));
        assert!(result_default.contains("content truncated"));

        let result_custom = truncate_content(&content, 3000, Some(2000), Some(500));
        let head_part: String = result_custom.chars().take(2000).collect();
        assert_eq!(head_part, "a".repeat(2000));
        assert!(result_custom.ends_with(&"a".repeat(500)));

        let big_content = "b".repeat(20_000);
        let result_micro = truncate_content(&big_content, 10_000, Some(6000), Some(3000));
        assert!(result_micro.starts_with(&"b".repeat(6000)));
        assert!(result_micro.ends_with(&"b".repeat(3000)));
        assert!(result_micro.contains("content truncated"));
    }

    #[test]
    fn test_truncate_scales_oversized_head_tail_requests() {
        let content = "c".repeat(4_000);
        let result = truncate_content(&content, 400, Some(6_000), Some(3_000));

        assert!(result.contains("content truncated"));
        assert!(result.len() < content.len());
    }

    #[test]
    fn test_summarize_write_file() {
        let input = serde_json::json!({
            "path": "src/main.rs",
            "content": "fn main() { println!(\"hello\"); }"
        });
        let result = summarize_write_input("write_file", &input).unwrap();
        assert_eq!(result["path"], "src/main.rs");
        assert_eq!(result["content"], "<<<AURA_ELIDED_CONTENT::32_bytes>>>");
        assert!(result.get("_summarized").is_none());
    }

    #[test]
    fn test_summarize_edit_file() {
        let input = serde_json::json!({
            "path": "src/lib.rs",
            "old_text": "old content here",
            "new_text": "new"
        });
        let result = summarize_write_input("edit_file", &input).unwrap();
        assert_eq!(result["path"], "src/lib.rs");
        assert_eq!(result["old_text"], "<<<AURA_ELIDED_OLD::16_chars>>>");
        assert_eq!(result["new_text"], "<<<AURA_ELIDED_NEW::3_chars>>>");
        assert!(result.get("_summarized").is_none());

        let input_alt = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "abc",
            "new_string": "defgh"
        });
        let result2 = summarize_write_input("edit_file", &input_alt).unwrap();
        assert_eq!(result2["old_string"], "<<<AURA_ELIDED_OLD::3_chars>>>");
        assert_eq!(result2["new_string"], "<<<AURA_ELIDED_NEW::5_chars>>>");
        assert!(result2.get("old_text").is_none());
        assert!(result2.get("new_text").is_none());
    }

    #[test]
    fn test_summarize_unknown_tool() {
        let input = serde_json::json!({"query": "some search"});
        assert!(summarize_write_input("search_code", &input).is_none());
        assert!(summarize_write_input("run_command", &input).is_none());
        assert!(summarize_write_input("totally_unknown", &input).is_none());
    }

    #[test]
    fn test_summarize_cached_tool_result_for_large_read_file() {
        let input = serde_json::json!({"path": "src/lib.rs"});
        let content = "a".repeat(9_000);
        let summary = summarize_cached_tool_result("read_file", &input, &content).unwrap();
        assert!(summary.contains("Cached result reused"));
        assert!(summary.contains("path=src/lib.rs"));
        assert!(summary.contains("Full output was 9000 chars"));
        assert!(summary.contains("truncated"));
        assert!(summary.len() < content.len());
    }

    #[test]
    fn test_summarize_cached_tool_result_cuts_large_read_file_footprint_substantially() {
        let input = serde_json::json!({"path": "src/lib.rs"});
        let content = "a".repeat(9_000);
        let summary = summarize_cached_tool_result("read_file", &input, &content).unwrap();
        let saved_chars = content.len() - summary.len();
        assert!(summary.len() <= 4_300, "summary should stay compact");
        assert!(
            saved_chars >= 4_500,
            "expected at least 4.5k chars saved, got {saved_chars}"
        );
    }

    #[test]
    fn test_summarize_cached_tool_result_leaves_small_result_unchanged() {
        let input = serde_json::json!({"path": "src/lib.rs"});
        let content = "fn main() {}\n";
        assert!(summarize_cached_tool_result("read_file", &input, content).is_none());
    }

    #[test]
    fn test_summarize_cached_tool_result_ignores_unknown_tools() {
        let input = serde_json::json!({"command": "pwd"});
        let content = "x".repeat(10_000);
        assert!(summarize_cached_tool_result("run_command", &input, &content).is_none());
    }

    #[test]
    fn test_summarize_cached_tool_result_cuts_large_search_code_footprint() {
        let input = serde_json::json!({"pattern": "fn main", "path": "src"});
        let content = "b".repeat(6_000);
        let summary = summarize_cached_tool_result("search_code", &input, &content).unwrap();
        let saved_chars = content.len() - summary.len();
        assert!(summary.len() <= 2_300, "summary should stay compact");
        assert!(
            saved_chars >= 3_500,
            "expected at least 3.5k chars saved, got {saved_chars}"
        );
    }

    #[test]
    fn test_summarize_cached_tool_result_cuts_large_list_files_footprint() {
        let input = serde_json::json!({"path": "."});
        let content = "c".repeat(3_000);
        let summary = summarize_cached_tool_result("list_files", &input, &content).unwrap();
        let saved_chars = content.len() - summary.len();
        assert!(summary.len() <= 1_400, "summary should stay compact");
        assert!(
            saved_chars >= 1_500,
            "expected at least 1.5k chars saved, got {saved_chars}"
        );
    }

    #[test]
    fn truncate_messages_for_storage_caps_oversized_tool_result_text() {
        let big = "Z".repeat(SESSION_TOOL_BLOB_MAX_BYTES + 1_000);
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: ToolResultContent::Text(big),
                is_error: false,
            }],
        }];
        truncate_messages_for_storage(&mut messages);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => {
                    assert!(t.len() < SESSION_TOOL_BLOB_MAX_BYTES + 200);
                    assert!(t.contains("[truncated"));
                }
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn truncate_messages_for_storage_is_noop_for_small_blobs() {
        let small = "ok".to_string();
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: ToolResultContent::Text(small.clone()),
                is_error: false,
            }],
        }];
        truncate_messages_for_storage(&mut messages);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => assert_eq!(t, &small),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn truncate_messages_for_storage_caps_oversized_tool_result_json() {
        let items: Vec<Value> = (0..500)
            .map(|i| serde_json::json!({ "id": format!("agent-{i}"), "pad": "X".repeat(200) }))
            .collect();
        let big = Value::Array(items);
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_list_agents".into(),
                content: ToolResultContent::Json(big),
                is_error: false,
            }],
        }];
        truncate_messages_for_storage(&mut messages);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => {
                    assert!(t.len() < SESSION_TOOL_BLOB_MAX_BYTES + 200);
                    assert!(t.contains("[truncated"));
                }
                other => {
                    panic!("oversized Json should be collapsed to truncated Text, got {other:?}");
                }
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
}
