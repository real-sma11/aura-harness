//! Message-history compaction and summarization helpers.

use aura_reasoner::{ContentBlock, Message, ModelRequestKind, Role, ToolResultContent};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use tracing::debug;

/// Read-only tools whose results are safe to fold by `content_hash`.
/// Single source of truth lives in `aura_config::CACHEABLE_TOOLS`.
use aura_config::CACHEABLE_TOOLS as READ_ONLY_DEDUP_TOOLS;

use aura_config::{
    CHARS_PER_TOKEN, COMPACTION_TIER_30, COMPACTION_TIER_60, COMPACTION_TIER_AGGRESSIVE,
    COMPACTION_TIER_HISTORY, COMPACTION_TIER_MICRO,
};
const DEFAULT_SUMMARY_AT: f64 = 0.85;
const DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES: usize = 24 * 1024;
const PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES: usize = 48 * 1024;

/// Upper bound on the synthetic summary's target footprint (in chars).
///
/// Used as a sanity ceiling so a request-body-cap-less Chat run can't
/// ask for an arbitrarily large summary target. 96 KiB ≈ 24 K tokens,
/// which is well under any sensible context budget.
const SUMMARY_TARGET_CEILING: usize = 96 * 1024;

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
    /// Current message-context estimate in tokens, when tracked separately.
    pub current_context_tokens: Option<u64>,
    /// Response token reserve included in pressure calculations.
    pub reserved_output_tokens: u64,
    /// Raw message bytes/chars used as a proxy-envelope pressure signal.
    pub raw_message_bytes: Option<usize>,
    /// Request contract kind, used to apply known body-size expectations.
    pub request_kind: Option<ModelRequestKind>,
    /// Explicit request body cap, when the caller knows one.
    pub request_body_cap_bytes: Option<usize>,
    /// Pressure at which local compaction asks the caller for model-backed summary escalation.
    pub summary_at: f64,
}

impl CompactionPolicy {
    /// Build the policy used by the agent loop from existing token estimates.
    #[must_use]
    pub fn new(
        max_context_tokens: Option<u64>,
        estimated_context_tokens: u64,
        reserved_output_tokens: u64,
    ) -> Self {
        Self {
            max_context_tokens,
            estimated_context_tokens,
            reserved_output_tokens,
            ..Self::default()
        }
    }

    const fn default_values() -> Self {
        Self {
            max_context_tokens: None,
            estimated_context_tokens: 0,
            current_context_tokens: None,
            reserved_output_tokens: 0,
            raw_message_bytes: None,
            request_kind: None,
            request_body_cap_bytes: None,
            summary_at: DEFAULT_SUMMARY_AT,
        }
    }
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        let mut policy = Self::default_values();
        policy.summary_at = env_unit_f64("AURA_COMPACTION_SUMMARY_AT", DEFAULT_SUMMARY_AT);
        policy
    }
}

fn env_unit_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .unwrap_or(default)
}

/// Mutable input bundle for message compaction.
pub struct CompactionInput<'a> {
    /// Messages to compact in place.
    pub messages: &'a mut [Message],
    /// Tier-selection policy.
    pub policy: CompactionPolicy,
}

/// Report returned by message compaction operations.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub enum CompactionAction {
    /// No compaction tier was selected.
    None,
    /// A tier was selected and applied.
    Applied(CompactionConfig),
    /// Local compaction ran, but the caller should ask a model to summarize middle history.
    NeedsSummary(SummaryInput),
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

impl RedactionMarker {
    /// Return a copy of `input` with `field` removed and structured redaction metadata added.
    #[must_use]
    pub fn mark(input: &Value, field: &str, bytes: usize) -> Value {
        let Value::Object(source) = input else {
            return input.clone();
        };

        let mut marked = source.clone();
        marked.remove(field);
        let entry = serde_json::json!({
            "kind": "aura_compaction_redaction",
            "version": 1,
            "field": field,
            "bytes": bytes,
        });

        match marked.remove("_redacted") {
            Some(Value::Object(mut existing)) => {
                if let Some(Value::Array(mut fields)) = existing.remove("fields") {
                    fields.push(entry_without_kind(field, bytes));
                    marked.insert(
                        "_redacted".to_string(),
                        serde_json::json!({
                            "kind": "aura_compaction_redaction",
                            "version": 1,
                            "fields": fields,
                        }),
                    );
                } else if let (Some(existing_field), Some(existing_bytes)) = (
                    existing.get("field").and_then(Value::as_str),
                    existing.get("bytes").and_then(Value::as_u64),
                ) {
                    marked.insert(
                        "_redacted".to_string(),
                        serde_json::json!({
                            "kind": "aura_compaction_redaction",
                            "version": 1,
                            "fields": [
                                { "field": existing_field, "bytes": existing_bytes },
                                { "field": field, "bytes": bytes },
                            ],
                        }),
                    );
                } else {
                    marked.insert("_redacted".to_string(), entry);
                }
            }
            Some(other) => {
                marked.insert("_redacted".to_string(), other);
            }
            None => {
                marked.insert("_redacted".to_string(), entry);
            }
        }

        Value::Object(marked)
    }

    /// Detect the structured redaction marker convention.
    #[must_use]
    pub fn is_marker(value: &Value) -> bool {
        let Value::Object(map) = value else {
            return false;
        };
        if let Some(marker) = map.get("_redacted") {
            return Self::is_marker(marker);
        }
        if map
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "aura_compaction_redaction")
        {
            return map.get("field").and_then(Value::as_str).is_some()
                || map.get("fields").and_then(Value::as_array).is_some();
        }
        false
    }
}

fn entry_without_kind(field: &str, bytes: usize) -> Value {
    serde_json::json!({ "field": field, "bytes": bytes })
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

/// Input for model-assisted history summary escalation.
#[derive(Debug, Clone)]
pub struct SummaryInput {
    /// Start index of the compactable middle history in the live message vector.
    pub compact_start: usize,
    /// Exclusive end index of the compactable middle history in the live message vector.
    pub compact_end: usize,
    /// Middle history that should be summarized by the caller's model provider.
    pub compactable_messages: Vec<Message>,
    /// Recent tail that must remain available verbatim after summary application.
    pub recent_tail: Vec<Message>,
    /// Estimated characters in the live transcript before summary replacement.
    pub original_chars: usize,
    /// Estimated characters after local compaction/redaction.
    pub local_chars: usize,
    /// Target total transcript size after the synthetic summary is applied.
    pub target_total_chars: usize,
    /// Suggested upper bound for the generated summary text.
    pub max_summary_chars: usize,
}

/// Input to a write-input or cached-result summary operation.
#[derive(Debug, Clone, Copy)]
pub struct ToolSummaryInput<'a> {
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
    /// Replacement text for compacted middle history.
    Messages {
        /// Model-generated summary for compacted middle history.
        text: String,
        /// Start index of the compacted history range.
        compact_start: usize,
        /// Exclusive end index of the compacted history range.
        compact_end: usize,
    },
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
    tier_for(utilization)
}

fn tier_for(pressure: f64) -> Option<CompactionConfig> {
    if pressure >= COMPACTION_TIER_HISTORY {
        Some(CompactionConfig::micro())
    } else if pressure >= COMPACTION_TIER_AGGRESSIVE {
        Some(CompactionConfig::aggressive())
    } else if pressure >= COMPACTION_TIER_60 {
        Some(CompactionConfig::moderate())
    } else if pressure >= COMPACTION_TIER_30 {
        Some(CompactionConfig::light())
    } else if pressure >= COMPACTION_TIER_MICRO {
        Some(CompactionConfig::history())
    } else {
        None
    }
}

/// Compute the effective pressure used for tier selection.
///
/// Pressure is the maximum of two real signals:
///
/// * **Context pressure** — `current_context_tokens / max_context_tokens`.
///   This is the only honest measure of how full the model's context
///   window is.
/// * **Request-body-cap pressure** — `raw_message_bytes / request_body_cap`,
///   used only on request kinds that actually have an upstream body
///   cap (`DevLoopBootstrap`, `ProjectToolSpecGen`, etc.). Plain `Chat`
///   continuations have no cap so this term is zero.
///
/// We deliberately do *not* drive pressure off raw message-byte count
/// alone — every modern model we target has a context window measured
/// in hundreds of thousands of tokens, and synthetic byte-envelope
/// pressure was silently rewriting tool results long before real
/// pressure existed.
#[must_use]
pub fn effective_pressure(input: &CompactionInput<'_>) -> f64 {
    let policy = input.policy;
    let context_pressure = policy.max_context_tokens.map_or(0.0, |max_ctx| {
        if max_ctx == 0 {
            return 0.0;
        }
        let context_tokens = policy
            .current_context_tokens
            .unwrap_or(policy.estimated_context_tokens)
            .max(policy.estimated_context_tokens);
        let pressure_tokens = context_tokens
            .saturating_add(policy.reserved_output_tokens)
            .min(max_ctx);
        #[allow(clippy::cast_precision_loss)]
        {
            pressure_tokens as f64 / max_ctx as f64
        }
    });
    let request_cap_pressure = request_body_cap(policy).map_or(0.0, |cap| {
        let raw_bytes = policy
            .raw_message_bytes
            .unwrap_or_else(|| estimate_message_chars(input.messages));
        cap_pressure(raw_bytes, cap)
    });

    context_pressure.max(request_cap_pressure).min(1.0)
}

fn request_body_cap(policy: CompactionPolicy) -> Option<usize> {
    policy
        .request_body_cap_bytes
        .or_else(|| match policy.request_kind? {
            ModelRequestKind::DevLoopBootstrap => Some(DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES),
            ModelRequestKind::ProjectToolSpecGen | ModelRequestKind::ProjectToolTaskExtract => {
                Some(PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES)
            }
            ModelRequestKind::Chat
            | ModelRequestKind::DevLoopContinuation
            | ModelRequestKind::Auxiliary => None,
        })
}

fn cap_pressure(bytes: usize, cap: usize) -> f64 {
    if cap == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / cap as f64
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

fn summary_target_total_chars(policy: CompactionPolicy, before_chars: usize) -> usize {
    let mut target = before_chars.saturating_mul(7) / 10;

    if let Some(max_ctx) = policy.max_context_tokens {
        let available_tokens = max_ctx.saturating_sub(policy.reserved_output_tokens);
        let available_chars = usize::try_from(available_tokens)
            .unwrap_or(usize::MAX / CHARS_PER_TOKEN)
            .saturating_mul(CHARS_PER_TOKEN);
        target = target.min(available_chars.saturating_mul(8) / 10);
    }

    if let Some(cap) = request_body_cap(policy) {
        target = target.min(cap.saturating_mul(8) / 10);
    }

    target.clamp(1_024, SUMMARY_TARGET_CEILING)
}

fn message_tool_use_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn message_tool_result_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

fn result_is_for_previous_tool_use(messages: &[Message], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    let result_ids = message_tool_result_ids(&messages[idx]);
    if result_ids.is_empty() {
        return false;
    }
    let previous_use_ids = message_tool_use_ids(&messages[idx - 1]);
    result_ids
        .iter()
        .any(|result_id| previous_use_ids.iter().any(|use_id| use_id == result_id))
}

fn boundary_splits_tool_pair(messages: &[Message], end: usize) -> bool {
    end < messages.len() && result_is_for_previous_tool_use(messages, end)
}

fn select_summary_range(messages: &[Message]) -> Option<(usize, usize)> {
    if messages.len() <= CompactionConfig::micro().preserve_recent + 2 {
        return None;
    }

    let mut start = 1;
    let mut end = messages
        .len()
        .saturating_sub(CompactionConfig::micro().preserve_recent);

    while start < end && result_is_for_previous_tool_use(messages, start) {
        start += 1;
    }
    while start < end && boundary_splits_tool_pair(messages, end) {
        end = end.saturating_sub(1);
    }

    (end.saturating_sub(start) >= 4).then_some((start, end))
}

fn build_summary_input(
    messages: &[Message],
    policy: CompactionPolicy,
    before_chars: usize,
    local_chars: usize,
) -> Option<SummaryInput> {
    let target_total_chars = summary_target_total_chars(policy, before_chars);
    if local_chars <= target_total_chars {
        return None;
    }

    let (compact_start, compact_end) = select_summary_range(messages)?;
    let compactable_messages = messages[compact_start..compact_end].to_vec();
    let compactable_chars = estimate_message_chars(&compactable_messages);
    if compactable_chars == 0 {
        return None;
    }

    let recent_tail = messages[compact_end..].to_vec();
    let max_summary_chars = compactable_chars
        .saturating_div(4)
        .min(target_total_chars.saturating_div(4))
        .clamp(512, 12_000);

    Some(SummaryInput {
        compact_start,
        compact_end,
        compactable_messages,
        recent_tail,
        original_chars: before_chars,
        local_chars,
        target_total_chars,
        max_summary_chars,
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
                *content = ToolResultContent::Text(truncate_content(
                    &text,
                    config.tool_result_max_chars,
                    head_chars,
                    tail_chars,
                ));
            }
        }
        ContentBlock::Text { text } => {
            if text.len() > config.text_max_chars {
                *text = truncate_content(text, config.text_max_chars, head_chars, tail_chars);
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

/// Stable hex digest matching the one stamped by
/// `aura_tools::fs_tools::read::content_hash_hex`. Re-derived locally
/// from the rendered ToolResult bytes because compaction sees the
/// `Message`-level shape (with no metadata side-channel) — and because
/// `DefaultHasher` is deterministic for the same bytes, the hash
/// produced here matches the one originally stamped by the read tool
/// for the same content.
fn dedup_content_hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Build a `tool_use_id -> tool_name` index by scanning every
/// `ContentBlock::ToolUse` in `messages`. The companion
/// `ContentBlock::ToolResult` only carries `tool_use_id`, so dedup
/// needs the index to filter by `READ_ONLY_DEDUP_TOOLS`.
fn tool_use_name_index(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

/// Look up the `path` argument from the `ContentBlock::ToolUse` whose
/// `id == tool_use_id`. Used to populate the `path` field of the dedup
/// marker so the model can still reason about which file the folded
/// read referred to. Returns `None` if the input has no `path` string
/// (e.g. `search_code`).
fn tool_use_input_path(messages: &[Message], tool_use_id: &str) -> Option<String> {
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, input, .. } = block {
                if id == tool_use_id {
                    return input.get("path").and_then(Value::as_str).map(String::from);
                }
            }
        }
    }
    None
}

/// Detect a previously-emitted dedup marker so a second pass over the
/// same `messages` doesn't fold it into a marker-of-a-marker. The
/// marker shape is `{ "_redacted": "<read_only_tool>", ... }`; any
/// other `_redacted` shape (e.g. the write-input shape produced by
/// [`RedactionMarker`]) is ignored here.
fn is_dedup_marker_text(text: &str) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(redacted) = map.get("_redacted").and_then(Value::as_str) else {
        return false;
    };
    READ_ONLY_DEDUP_TOOLS.contains(&redacted)
}

/// Collapse older read-only tool results whose `content_hash` matches
/// a later occurrence to a short structured marker, keeping the newest
/// copy verbatim. Returns the number of folds applied.
///
/// Walks newest-to-oldest. For each `ContentBlock::ToolResult` whose
/// matching `ContentBlock::ToolUse` names a tool in
/// [`READ_ONLY_DEDUP_TOOLS`], the helper hashes the rendered tool
/// output text and tracks `(tool_name, content_hash)` in a set. The
/// first time a hash is seen, the message is kept verbatim and the
/// hash is recorded; on every subsequent (older) occurrence of the
/// same `(tool_name, content_hash)` pair, the tool result's text is
/// replaced with a one-line JSON marker:
///
/// ```json
/// {"_redacted":"read_file","path":"<path>","content_hash":"<hash>","note":"see later identical read"}
/// ```
///
/// The message structure (role, `tool_use_id`, `is_error`) is left
/// untouched so the assistant/tool-result pairing the Anthropic API
/// requires stays intact — only the text content shrinks. Existing
/// dedup markers are detected via [`is_dedup_marker_text`] and skipped
/// so re-running compaction does not collapse markers into
/// markers-of-markers.
///
/// Exposed publicly so the Phase 6 Shamir replay harness in
/// `aura-agent` can drive the same dedup pass against its captured
/// transcript without going through the full `compact_messages`
/// pressure-tier dance. Production callers should still go through
/// [`compact_messages`], which is the only path that decides whether
/// dedup should fire for a given pressure.
pub fn dedup_read_results_by_content_hash(messages: &mut [Message]) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let name_index = tool_use_name_index(messages);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut folded = 0usize;

    let len = messages.len();
    for i in (0..len).rev() {
        let block_count = messages[i].content.len();
        for block_idx in 0..block_count {
            let (tool_use_id, rendered_text) = {
                let block = &messages[i].content[block_idx];
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                else {
                    continue;
                };
                let text = match content {
                    ToolResultContent::Text(t) => t.clone(),
                    ToolResultContent::Json(v) => serde_json::to_string(v).unwrap_or_default(),
                };
                (tool_use_id.clone(), text)
            };

            if is_dedup_marker_text(&rendered_text) {
                continue;
            }

            let Some(tool_name) = name_index.get(&tool_use_id).cloned() else {
                continue;
            };
            if !READ_ONLY_DEDUP_TOOLS.contains(&tool_name.as_str()) {
                continue;
            }

            let hash = dedup_content_hash_hex(rendered_text.as_bytes());
            let key = (tool_name.clone(), hash.clone());
            if seen.contains(&key) {
                let path = tool_use_input_path(messages, &tool_use_id).unwrap_or_default();
                let marker = serde_json::json!({
                    "_redacted": tool_name,
                    "path": path,
                    "content_hash": hash,
                    "note": "see later identical read",
                });
                if let ContentBlock::ToolResult { content, .. } =
                    &mut messages[i].content[block_idx]
                {
                    *content = ToolResultContent::Text(marker.to_string());
                    folded += 1;
                }
            } else {
                seen.insert(key);
            }
        }
    }
    folded
}

/// Choose and apply a compaction tier using context-window utilization
/// and explicit request-body caps.
#[allow(clippy::needless_pass_by_value)]
pub fn compact_messages(input: CompactionInput<'_>) -> CompactionReport {
    let before_chars = estimate_message_chars(input.messages);
    let pressure = effective_pressure(&input);
    let chosen = tier_for(pressure);
    let policy = input.policy;

    if let Some(tier) = chosen {
        debug!(
            pressure,
            tool_result_max_chars = tier.tool_result_max_chars,
            text_max_chars = tier.text_max_chars,
            preserve_recent = tier.preserve_recent,
            "Compacting context"
        );
        // Phase 2: fold older read-only tool results that the model
        // has already re-read more recently. Runs before
        // `compact_older_messages` so the truncation step never sees
        // (and therefore never re-redacts) an already-folded marker.
        let folded = dedup_read_results_by_content_hash(input.messages);
        if folded > 0 {
            debug!(
                folded,
                "content_hash dedup folded older read-only tool results"
            );
        }
        compact_older_messages(input.messages, &tier);
    }

    let after_chars = estimate_message_chars(input.messages);
    let action = if pressure >= policy.summary_at {
        build_summary_input(input.messages, policy, before_chars, after_chars).map_or_else(
            || chosen.map_or(CompactionAction::None, CompactionAction::Applied),
            CompactionAction::NeedsSummary,
        )
    } else {
        chosen.map_or(CompactionAction::None, CompactionAction::Applied)
    };

    CompactionReport {
        before_chars,
        after_chars,
        action,
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

/// Rewrite compactable middle history to a single synthetic summary message.
pub fn apply_message_summary(
    messages: &mut Vec<Message>,
    summary: SummaryOutput,
) -> CompactionReport {
    let before_chars = estimate_message_chars(messages);
    let SummaryOutput::Messages {
        text,
        compact_start,
        compact_end,
    } = summary
    else {
        return CompactionReport {
            before_chars,
            after_chars: before_chars,
            action: CompactionAction::None,
        };
    };

    if text.trim().is_empty()
        || compact_start == 0
        || compact_start >= compact_end
        || compact_end > messages.len()
        || result_is_for_previous_tool_use(messages, compact_start)
        || boundary_splits_tool_pair(messages, compact_end)
    {
        return CompactionReport {
            before_chars,
            after_chars: before_chars,
            action: CompactionAction::None,
        };
    }

    let removed_chars = estimate_message_chars(&messages[compact_start..compact_end]);
    let max_summary_chars = removed_chars.saturating_sub(1).clamp(256, 12_000);
    let summary_text = if text.len() > max_summary_chars {
        truncate_content(&text, max_summary_chars, None, None)
    } else {
        text
    };
    let synthetic = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: format!(
                "[summary of compacted prior conversation]\n\n{}",
                summary_text.trim()
            ),
        }],
    };

    messages.splice(compact_start..compact_end, [synthetic]);

    let mut after_chars = estimate_message_chars(messages);
    if after_chars >= before_chars && messages.len() > compact_start {
        let fallback_cap = removed_chars.saturating_div(4).clamp(128, 4_000);
        let fallback = truncate_content(
            messages[compact_start].text_content().as_str(),
            fallback_cap,
            None,
            None,
        );
        messages[compact_start] = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: fallback }],
        };
        after_chars = estimate_message_chars(messages);
    }

    CompactionReport {
        before_chars,
        after_chars,
        action: CompactionAction::Applied(CompactionConfig::micro()),
    }
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
            let content_len = input
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            if input.get("content").is_some() {
                Some(RedactionMarker::mark(input, "content", content_len))
            } else {
                None
            }
        }
        "edit_file" => {
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
            if input.get(old_key).is_none() || input.get(new_key).is_none() {
                return None;
            }
            let old_len = input
                .get(old_key)
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let new_len = input
                .get(new_key)
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let redacted = RedactionMarker::mark(input, old_key, old_len);
            Some(RedactionMarker::mark(&redacted, new_key, new_len))
        }
        _ => None,
    }
}

/// Apply a write-input redaction summary.
///
/// Cached-result-shaping has been removed: tool results either fit
/// verbatim or get a visible `truncate_content` marker via
/// `compact_older_messages`. This entry point now only redacts oversized
/// write-tool inputs that the storage / request-build path hands us.
#[must_use]
pub fn apply_summary(input: ToolSummaryInput<'_>) -> Option<SummaryOutput> {
    summarize_write_input(input.tool_name, input.input).map(SummaryOutput::Input)
}

/// Cap each `ToolUse` input / `ToolResult` content in `messages` at the storage limit.
///
/// Invariant: `tool_use.input` MUST remain a JSON object on the wire — the
/// Anthropic Messages API rejects anything else with 400
/// `messages.N.content.M.tool_use.input: Input should be an object`.
/// Oversized inputs are redacted in-place via `RedactionMarker`, which drops
/// the largest top-level string field(s) and records a structured
/// `_redacted` summary, preserving the object shape.
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
                    redact_oversized_tool_use_input(input, SESSION_TOOL_BLOB_MAX_BYTES);
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

/// Shrink an oversized `tool_use.input` in place while keeping it a JSON
/// object. Walks the top-level string fields in descending size order and
/// replaces each with a `RedactionMarker` summary until the serialized size
/// fits under `cap`. Non-object inputs and inputs already carrying a
/// redaction marker are left untouched: the former are invalid upstream
/// (and surfaced by the aura-os persistence guard), the latter are already
/// minimized.
fn redact_oversized_tool_use_input(input: &mut Value, cap: usize) {
    if !input.is_object() {
        return;
    }
    if RedactionMarker::is_marker(input) {
        return;
    }
    let Ok(mut serialized_len) = serde_json::to_string(input).map(|s| s.len()) else {
        return;
    };
    while serialized_len > cap {
        let largest = input.as_object().and_then(|m| {
            m.iter()
                .filter(|(k, v)| {
                    k.as_str() != "_redacted" && v.as_str().is_some_and(|s| !s.is_empty())
                })
                .max_by_key(|(_, v)| v.as_str().map_or(0, str::len))
                .map(|(k, v)| (k.clone(), v.as_str().map_or(0, str::len)))
        });
        let Some((field, bytes)) = largest else {
            return;
        };
        *input = RedactionMarker::mark(input, &field, bytes);
        let Ok(new_len) = serde_json::to_string(input).map(|s| s.len()) else {
            return;
        };
        if new_len >= serialized_len {
            return;
        }
        serialized_len = new_len;
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
        assert!(result.get("content").is_none());
        assert_eq!(result["_redacted"]["field"], "content");
        assert_eq!(result["_redacted"]["bytes"], 32);
        assert!(RedactionMarker::is_marker(&result));
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
        assert!(result.get("old_text").is_none());
        assert!(result.get("new_text").is_none());
        assert_eq!(result["_redacted"]["fields"][0]["field"], "old_text");
        assert_eq!(result["_redacted"]["fields"][0]["bytes"], 16);
        assert_eq!(result["_redacted"]["fields"][1]["field"], "new_text");
        assert_eq!(result["_redacted"]["fields"][1]["bytes"], 3);
        assert!(RedactionMarker::is_marker(&result));

        let input_alt = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "abc",
            "new_string": "defgh"
        });
        let result2 = summarize_write_input("edit_file", &input_alt).unwrap();
        assert!(result2.get("old_string").is_none());
        assert!(result2.get("new_string").is_none());
        assert_eq!(result2["_redacted"]["fields"][0]["field"], "old_string");
        assert_eq!(result2["_redacted"]["fields"][1]["field"], "new_string");
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

    fn tool_use_with_id(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id,
                "read_file",
                serde_json::json!({"path": "big.rs"}),
            )],
        }
    }

    fn tool_result_with_id(id: &str, content: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(
                id,
                ToolResultContent::Text(content.into()),
                false,
            )],
        }
    }

    fn summary_pressure_policy(before_chars: usize) -> CompactionPolicy {
        CompactionPolicy {
            raw_message_bytes: Some(before_chars),
            request_body_cap_bytes: Some(8_000),
            summary_at: 0.85,
            ..CompactionPolicy::default()
        }
    }

    fn long_summary_fixture() -> Vec<Message> {
        let mut messages = vec![Message::user("anchor")];
        for i in 0..20 {
            if i % 2 == 0 {
                messages.push(Message::assistant("A".repeat(10_000)));
            } else {
                messages.push(Message::user("B".repeat(10_000)));
            }
        }
        messages.push(Message::assistant("recent assistant tail"));
        messages.push(Message::user("recent user tail"));
        messages
    }

    #[test]
    fn summary_action_preserves_recent_tail() {
        let mut messages = long_summary_fixture();
        let before_chars = estimate_message_chars(&messages);

        let report = compact_messages(CompactionInput {
            messages: &mut messages,
            policy: summary_pressure_policy(before_chars),
        });

        let CompactionAction::NeedsSummary(input) = report.action else {
            panic!("expected summary escalation, got {:?}", report.action);
        };
        assert_eq!(input.recent_tail.len(), 2);
        assert_eq!(input.recent_tail[0].text_content(), "recent assistant tail");
        assert_eq!(input.recent_tail[1].text_content(), "recent user tail");
        assert_eq!(
            messages[messages.len() - 2].text_content(),
            "recent assistant tail"
        );
        assert_eq!(
            messages[messages.len() - 1].text_content(),
            "recent user tail"
        );
    }

    #[test]
    fn summary_action_preserves_tool_adjacency() {
        let mut messages = vec![Message::user("anchor")];
        for i in 0..16 {
            if i % 2 == 0 {
                messages.push(Message::assistant("A".repeat(10_000)));
            } else {
                messages.push(Message::user("B".repeat(10_000)));
            }
        }
        messages.push(tool_use_with_id("toolu_boundary"));
        messages.push(tool_result_with_id("toolu_boundary", "result tail"));
        messages.push(Message::assistant("latest"));

        let before_chars = estimate_message_chars(&messages);
        let report = compact_messages(CompactionInput {
            messages: &mut messages,
            policy: summary_pressure_policy(before_chars),
        });

        let CompactionAction::NeedsSummary(input) = report.action else {
            panic!("expected summary escalation, got {:?}", report.action);
        };
        assert!(
            matches!(
                input.recent_tail.first().and_then(|message| message.content.first()),
                Some(ContentBlock::ToolUse { id, .. }) if id == "toolu_boundary"
            ),
            "summary range must not split a tool_use/tool_result pair"
        );
        assert!(
            matches!(
                input.recent_tail.get(1).and_then(|message| message.content.first()),
                Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == "toolu_boundary"
            ),
            "paired tool_result should stay adjacent to its tool_use"
        );
    }

    #[test]
    fn apply_summary_reduces_message_bytes() {
        let mut messages = vec![
            Message::user("anchor"),
            Message::assistant("middle assistant ".repeat(2_000)),
            Message::user("middle user ".repeat(2_000)),
            Message::assistant("recent tail"),
        ];
        let before_chars = estimate_message_chars(&messages);

        let report = apply_message_summary(
            &mut messages,
            SummaryOutput::Messages {
                text: "The compacted middle history discussed prior implementation details."
                    .to_string(),
                compact_start: 1,
                compact_end: 3,
            },
        );

        assert!(report.after_chars < before_chars);
        assert!(report.reduced());
        assert_eq!(messages.len(), 3);
        assert!(messages[1]
            .text_content()
            .contains("compacted middle history"));
        assert_eq!(messages[2].text_content(), "recent tail");
    }

    #[test]
    fn summary_action_only_at_high_pressure() {
        // Build a fixture that's well below the summary_at threshold:
        // 200K context window, 80K context tokens (40% pressure) — far
        // below the 0.85 default. Compaction may still apply a tier,
        // but it must NOT escalate to NeedsSummary.
        let mut messages = long_summary_fixture();
        let report = compact_messages(CompactionInput {
            messages: &mut messages,
            policy: CompactionPolicy {
                max_context_tokens: Some(200_000),
                current_context_tokens: Some(80_000),
                summary_at: 0.85,
                ..CompactionPolicy::default()
            },
        });

        assert!(
            !matches!(report.action, CompactionAction::NeedsSummary(_)),
            "summary escalation should wait for policy.summary_at"
        );
    }

    #[test]
    fn marker_is_not_a_string_field() {
        let input = serde_json::json!({
            "path": "src/main.rs",
            "content": "fn main() {}",
        });
        let marked = RedactionMarker::mark(&input, "content", 12);

        assert!(marked.get("content").is_none());
        assert!(marked.get("_redacted").is_some_and(Value::is_object));
        assert!(RedactionMarker::is_marker(&marked));
    }

    #[test]
    fn request_kind_body_cap_contributes_pressure() {
        // Without `absolute_byte_pressure`, the `DevLoopBootstrap` body
        // cap is the only thing that can push pressure above zero on a
        // tiny conversation. Set raw_message_bytes well above the
        // 24 KiB cap and assert pressure crosses into a tier purely on
        // the cap math (raw / cap), with `max_context_tokens` and
        // `current_context_tokens` left at their defaults.
        let mut messages = vec![Message::user("small")];
        let input = CompactionInput {
            messages: &mut messages,
            policy: CompactionPolicy {
                raw_message_bytes: Some(DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES + 1),
                request_kind: Some(ModelRequestKind::DevLoopBootstrap),
                ..CompactionPolicy::default()
            },
        };

        let pressure = effective_pressure(&input);
        assert!(
            pressure >= 1.0,
            "raw bytes above the cap should saturate request_cap_pressure to 1.0; got {pressure}"
        );
        assert!(
            select_tier(pressure).is_some(),
            "saturated pressure should pick a tier"
        );

        // Sanity: a Chat continuation has no body cap, so the same
        // raw_message_bytes should NOT trip pressure on its own.
        let mut messages2 = vec![Message::user("small")];
        let chat_input = CompactionInput {
            messages: &mut messages2,
            policy: CompactionPolicy {
                raw_message_bytes: Some(DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES + 1),
                request_kind: Some(ModelRequestKind::Chat),
                ..CompactionPolicy::default()
            },
        };
        assert_eq!(
            effective_pressure(&chat_input),
            0.0,
            "Chat continuations have no body cap; raw bytes alone must not produce pressure",
        );
    }

    /// Regression: a Rust-shaped 6 KB `read_file` ToolResult must
    /// survive `compact_messages` verbatim when no compaction tier is
    /// selected, and at every pressure below `summary_at` (default
    /// 0.85) the worst case must be a *visible* `truncate_content`
    /// marker — never a silent signature-compact skeleton with
    /// `// ... body omitted ...`.
    ///
    /// (The plan's wording said "verbatim at 40% pressure", but the
    /// existing tier matrix selects the `light` tier at >= 30% with
    /// `tool_result_max_chars = 3000`, so a 6 KB ToolResult is
    /// always truncated above the 30% threshold. The regression we
    /// actually care about is *signature-compact never fires at any
    /// non-summary pressure*; that's what this test pins down.)
    #[test]
    fn read_file_result_survives_compaction_below_summary_pressure() {
        // ~6 KB of Rust-shaped content. The fixture deliberately
        // contains the structural markers `try_signature_compact` used
        // to look for, so any future regression that re-introduces
        // signature collapse will produce a `body omitted` marker that
        // this test asserts is absent.
        let snippet = "pub fn foo(x: u64) -> u64 {\n    let y = x + 1;\n    y * 2\n}\n\n";
        let body = snippet.repeat(120);
        assert!(body.len() >= 6_000, "fixture should be at least 6 KB");

        let original_len = body.len();
        let original_body = body.clone();

        fn build_history(body: &str) -> Vec<Message> {
            let mut messages = Vec::new();
            messages.push(Message::user("anchor"));
            // A few small messages so the ToolResult is *not* in the
            // preserve_recent tail.
            for i in 0..4 {
                messages.push(Message::assistant(format!("filler-{i}")));
                messages.push(Message::user(format!("ack-{i}")));
            }
            // The aged ToolResult under test.
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(
                    "tu_read_publisher",
                    ToolResultContent::Text(body.to_string()),
                    false,
                )],
            });
            // Pad the tail to push the ToolResult above out of
            // preserve_recent.
            for i in 0..12 {
                messages.push(Message::assistant(format!("tail-asst-{i}")));
                messages.push(Message::user(format!("tail-user-{i}")));
            }
            messages
        }

        // The aged ToolResult is at index `1 + 4*2 = 9`.
        let target_idx = 9;

        // 10% pressure: below `select_tier`'s 15% floor, so no
        // compaction tier fires. The ToolResult must be byte-for-byte
        // identical to the original.
        {
            let mut messages = build_history(&body);
            assert!(matches!(
                &messages[target_idx].content[0],
                ContentBlock::ToolResult { .. }
            ));

            compact_messages(CompactionInput {
                messages: &mut messages,
                policy: CompactionPolicy {
                    max_context_tokens: Some(200_000),
                    current_context_tokens: Some(20_000),
                    ..CompactionPolicy::default()
                },
            });

            match &messages[target_idx].content[0] {
                ContentBlock::ToolResult { content, .. } => match content {
                    ToolResultContent::Text(t) => {
                        assert_eq!(
                            t.len(),
                            original_len,
                            "at 10% pressure the ToolResult must survive verbatim"
                        );
                        assert_eq!(t, &original_body);
                    }
                    other => panic!("expected Text content, got {other:?}"),
                },
                other => panic!("expected ToolResult, got {other:?}"),
            }
        }

        // 40%, 65%, 80% pressure: compaction may shrink the result,
        // but the worst case must be a visible `truncate_content`
        // marker — never the old signature-compact
        // `// ... body omitted ...` skeleton, and never the old
        // cached-result-shaping "Cached result reused" preamble.
        for pct in [0.40_f64, 0.65_f64, 0.80_f64] {
            let mut messages = build_history(&body);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let current_tokens = (200_000.0_f64 * pct) as u64;

            compact_messages(CompactionInput {
                messages: &mut messages,
                policy: CompactionPolicy {
                    max_context_tokens: Some(200_000),
                    current_context_tokens: Some(current_tokens),
                    ..CompactionPolicy::default()
                },
            });

            match &messages[target_idx].content[0] {
                ContentBlock::ToolResult { content, .. } => match content {
                    ToolResultContent::Text(t) => {
                        if t.len() == original_len {
                            assert_eq!(
                                t, &original_body,
                                "untouched results must be byte-identical"
                            );
                        } else {
                            assert!(
                                t.contains("content truncated"),
                                "shrunk ToolResult at {pct} pressure must carry a visible \
                                 truncate_content marker; got prefix: {}",
                                &t.chars().take(200).collect::<String>()
                            );
                            assert!(
                                !t.contains("body omitted"),
                                "shrunk ToolResult must NOT use signature-compact skeleton; \
                                 got at {pct} pressure prefix: {}",
                                &t.chars().take(200).collect::<String>()
                            );
                            assert!(
                                !t.contains("Cached result reused"),
                                "shrunk ToolResult must NOT use cached-result-shaping prefix; \
                                 got at {pct} pressure prefix: {}",
                                &t.chars().take(200).collect::<String>()
                            );
                        }
                    }
                    other => panic!("expected Text content, got {other:?}"),
                },
                other => panic!("expected ToolResult, got {other:?}"),
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase 2: content_hash dedup of read-only tool results.
    //
    // The fixtures below build hand-crafted ToolUse/ToolResult message
    // pairs (rather than going through the agent loop) so the dedup
    // helper can be exercised in isolation. Each test pins down a
    // single contract documented in the matching `assert!`.
    // ------------------------------------------------------------------

    fn read_tool_use(id: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id,
                "read_file",
                serde_json::json!({ "path": path }),
            )],
        }
    }

    fn read_tool_result(id: &str, body: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(
                id,
                ToolResultContent::Text(body.to_string()),
                false,
            )],
        }
    }

    fn write_tool_use(id: &str, path: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id,
                "write_file",
                serde_json::json!({ "path": path, "content": "..." }),
            )],
        }
    }

    fn write_tool_result(id: &str, body: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(
                id,
                ToolResultContent::Text(body.to_string()),
                false,
            )],
        }
    }

    #[test]
    fn dedup_collapses_older_identical_read_keeps_newest() {
        let body = "fn main() {\n    println!(\"hi\");\n}\n".repeat(20);
        let mut messages = vec![
            read_tool_use("tu_older", "src/main.rs"),
            read_tool_result("tu_older", &body),
            Message::assistant("intermediate step"),
            read_tool_use("tu_newer", "src/main.rs"),
            read_tool_result("tu_newer", &body),
        ];

        let folded = dedup_read_results_by_content_hash(&mut messages);
        assert_eq!(folded, 1, "exactly the older identical read should fold");

        // Newer (last) read stays verbatim.
        match &messages[4].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => assert_eq!(t, &body, "newest read must be verbatim"),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // Older read becomes the structured marker.
        match &messages[1].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => {
                    let parsed: Value = serde_json::from_str(t)
                        .expect("marker must be valid JSON so downstream serde survives");
                    assert_eq!(parsed["_redacted"], "read_file");
                    assert_eq!(parsed["path"], "src/main.rs");
                    assert_eq!(parsed["note"], "see later identical read");
                    assert!(
                        parsed["content_hash"]
                            .as_str()
                            .is_some_and(|h| !h.is_empty()),
                        "marker must carry the content_hash of the folded read"
                    );
                    assert!(
                        t.len() < body.len() / 2,
                        "marker must be substantially shorter than the original"
                    );
                }
                other => panic!("expected Text marker, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn dedup_does_not_touch_write_tool_results() {
        // Two write_file tool results with byte-identical text. Even
        // though their hashes match, `write_file` is not in the
        // read-only dedup set so neither result must be folded.
        let body = "write succeeded: src/lib.rs (32 bytes)";
        let mut messages = vec![
            write_tool_use("tu_w1", "src/lib.rs"),
            write_tool_result("tu_w1", body),
            Message::assistant("step"),
            write_tool_use("tu_w2", "src/lib.rs"),
            write_tool_result("tu_w2", body),
        ];

        let folded = dedup_read_results_by_content_hash(&mut messages);
        assert_eq!(folded, 0, "write_file results must never be folded");

        for idx in [1usize, 4] {
            match &messages[idx].content[0] {
                ContentBlock::ToolResult { content, .. } => match content {
                    ToolResultContent::Text(t) => {
                        assert_eq!(t, body, "write_file result at idx {idx} must stay verbatim");
                    }
                    other => panic!("expected Text, got {other:?}"),
                },
                other => panic!("expected ToolResult, got {other:?}"),
            }
        }
    }

    #[test]
    fn dedup_preserves_message_count_and_tool_use_ids() {
        let body_a = "lots of bytes A".repeat(50);
        let body_b = "lots of bytes B".repeat(50);
        let mut messages = vec![
            read_tool_use("tu_a1", "a.rs"),
            read_tool_result("tu_a1", &body_a),
            read_tool_use("tu_b1", "b.rs"),
            read_tool_result("tu_b1", &body_b),
            Message::assistant("midway"),
            read_tool_use("tu_a2", "a.rs"),
            read_tool_result("tu_a2", &body_a),
            read_tool_use("tu_b2", "b.rs"),
            read_tool_result("tu_b2", &body_b),
        ];
        let before_len = messages.len();
        let before_use_ids: Vec<Option<String>> = messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::ToolUse { id, .. }) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let before_result_ids: Vec<Option<String>> = messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::ToolResult { tool_use_id, .. }) => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();

        let folded = dedup_read_results_by_content_hash(&mut messages);
        assert_eq!(folded, 2, "two older reads (one per body) should fold");
        assert_eq!(messages.len(), before_len, "length must be preserved");

        let after_use_ids: Vec<Option<String>> = messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::ToolUse { id, .. }) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let after_result_ids: Vec<Option<String>> = messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::ToolResult { tool_use_id, .. }) => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            after_use_ids, before_use_ids,
            "tool_use ids must remain in the same positions"
        );
        assert_eq!(
            after_result_ids, before_result_ids,
            "tool_use_ids on ToolResult blocks must remain in the same positions"
        );
    }

    #[test]
    fn dedup_respects_distinct_hashes() {
        // Two read_file results with *different* content. Neither
        // should be folded; both stay verbatim.
        let body_a = "alpha bytes".repeat(30);
        let body_b = "bravo bytes".repeat(30);
        let mut messages = vec![
            read_tool_use("tu_a", "a.rs"),
            read_tool_result("tu_a", &body_a),
            read_tool_use("tu_b", "b.rs"),
            read_tool_result("tu_b", &body_b),
        ];

        let folded = dedup_read_results_by_content_hash(&mut messages);
        assert_eq!(folded, 0, "distinct hashes must not be folded");

        match &messages[1].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => assert_eq!(t, &body_a),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
        match &messages[3].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => assert_eq!(t, &body_b),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected ToolResult, got {other:?}"),
        }
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
    fn truncate_messages_for_storage_keeps_oversized_tool_use_input_as_object() {
        // Regression: previously this branch wrote `Value::String(truncated)`
        // back into `tool_use.input`, which Anthropic rejects on replay with
        // 400 `messages.N.content.M.tool_use.input: Input should be an
        // object`. With the RedactionMarker fix, the input must remain a
        // JSON object and the oversized string field is summarized.
        let big_markdown = "M".repeat(SESSION_TOOL_BLOB_MAX_BYTES + 4_000);
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "tu_create_spec",
                "create_spec",
                serde_json::json!({
                    "title": "Phase 06: MLS GroupService",
                    "markdown_contents": big_markdown,
                }),
            )],
        }];
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] else {
            panic!("expected ToolUse, got {:?}", messages[0].content[0]);
        };
        assert!(input.is_object(), "input must remain a JSON object");
        assert!(
            RedactionMarker::is_marker(input),
            "oversized input should carry a redaction marker"
        );
        assert_eq!(
            input.get("title").and_then(Value::as_str),
            Some("Phase 06: MLS GroupService"),
            "small fields should survive"
        );
        assert!(
            input.get("markdown_contents").is_none(),
            "the oversized field should be removed"
        );
        let serialized = serde_json::to_string(input).expect("serialize");
        assert!(
            serialized.len() <= SESSION_TOOL_BLOB_MAX_BYTES,
            "redacted input ({} bytes) must fit under storage cap ({} bytes)",
            serialized.len(),
            SESSION_TOOL_BLOB_MAX_BYTES
        );
    }

    #[test]
    fn truncate_messages_for_storage_idempotent_for_already_redacted_tool_use() {
        // Running compaction twice should be a no-op once the input has
        // already been marked; otherwise we'd keep stacking markers.
        let big = "X".repeat(SESSION_TOOL_BLOB_MAX_BYTES + 2_000);
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "tu_1",
                "create_spec",
                serde_json::json!({
                    "title": "ok",
                    "markdown_contents": big,
                }),
            )],
        }];
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input: first, .. } = messages[0].content[0].clone() else {
            panic!("expected ToolUse after first pass");
        };
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input: second, .. } = &messages[0].content[0] else {
            panic!("expected ToolUse after second pass");
        };
        assert_eq!(&first, second, "second pass must be a no-op");
    }

    #[test]
    fn truncate_messages_for_storage_preserves_small_tool_use_input() {
        let input_value = serde_json::json!({
            "title": "Tiny",
            "markdown_contents": "# Hello\n\nworld",
        });
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "tu_small",
                "create_spec",
                input_value.clone(),
            )],
        }];
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] else {
            panic!("expected ToolUse");
        };
        assert_eq!(input, &input_value, "below-cap input must be untouched");
    }

    #[test]
    fn truncate_messages_for_storage_redacts_multiple_oversized_string_fields() {
        // `edit_file` carries both `old_text` and `new_text`; if a single
        // field redaction isn't enough we walk to the next-largest field.
        let big = "Q".repeat(SESSION_TOOL_BLOB_MAX_BYTES);
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "tu_edit",
                "edit_file",
                serde_json::json!({
                    "path": "src/big.rs",
                    "old_text": big.clone(),
                    "new_text": big,
                }),
            )],
        }];
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] else {
            panic!("expected ToolUse");
        };
        assert!(input.is_object());
        assert!(RedactionMarker::is_marker(input));
        assert_eq!(
            input.get("path").and_then(Value::as_str),
            Some("src/big.rs")
        );
        let serialized = serde_json::to_string(input).expect("serialize");
        assert!(
            serialized.len() <= SESSION_TOOL_BLOB_MAX_BYTES,
            "redacted input ({} bytes) must fit under storage cap",
            serialized.len()
        );
    }

    #[test]
    fn truncate_messages_for_storage_leaves_non_object_tool_use_input_untouched() {
        // Defensive: non-object inputs are already invalid upstream. We
        // intentionally don't mutate them so the upstream bug is visible
        // (and the aura-os persistence guard surfaces it).
        let original = Value::String("oops not an object".repeat(SESSION_TOOL_BLOB_MAX_BYTES / 4));
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                "tu_corrupt",
                "create_spec",
                original.clone(),
            )],
        }];
        truncate_messages_for_storage(&mut messages);
        let ContentBlock::ToolUse { input, .. } = &messages[0].content[0] else {
            panic!("expected ToolUse");
        };
        assert_eq!(input, &original);
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
