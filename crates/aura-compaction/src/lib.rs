//! Unified context compaction for the Aura agent loop.
//!
//! Phase 1 centralizes the existing message-history, storage, and tool-surface
//! compaction routines without changing their behavior. Callers still own
//! agent-loop state, sanitization, and intent/tool filtering; this crate owns
//! the pure compaction policies and mutations.

#![forbid(unsafe_code)]

mod messages;
mod tools;

pub use messages::{
    absolute_byte_tier, apply_message_summary, apply_summary, compact_exploration_if_needed,
    compact_for_storage, compact_messages, compact_older_messages, effective_pressure,
    estimate_message_chars, message_chars_to_tokens, pick_stricter_tier, recover_overflow,
    select_tier, summarize_cached_tool_result, summarize_write_input, truncate_content,
    truncate_messages_for_storage, try_signature_compact, CompactionAction, CompactionConfig,
    CompactionInput, CompactionPolicy, CompactionReport, OverflowStep, RedactionMarker,
    SummaryInput, SummaryOutput, ToolSummaryInput, ABSOLUTE_BYTE_AGGRESSIVE_AT,
    ABSOLUTE_BYTE_LIGHT_AT, ABSOLUTE_BYTE_MICRO_AT, SESSION_TOOL_BLOB_MAX_BYTES,
};
pub use tools::{
    compact_tool_surface, compact_tools, tool_definition_chars, tools_chars, ToolSurfaceReport,
};

/// Stateless facade for applying the crate's compaction APIs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Compactor;

impl Compactor {
    /// Create a compactor facade.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Choose and apply message compaction.
    pub fn compact_messages(&self, input: CompactionInput<'_>) -> CompactionReport {
        compact_messages(input)
    }

    /// Apply storage compaction to persisted messages.
    pub fn compact_for_storage(&self, messages: &mut [aura_reasoner::Message]) {
        compact_for_storage(messages);
    }

    /// Apply overflow recovery with a specific tier.
    pub fn recover_overflow(
        &self,
        messages: &mut [aura_reasoner::Message],
        tier: CompactionConfig,
    ) -> CompactionReport {
        recover_overflow(messages, tier)
    }

    /// Compact the tool surface supplied by the caller.
    pub fn compact_tool_surface(
        &self,
        tools: &mut [aura_reasoner::ToolDefinition],
    ) -> ToolSurfaceReport {
        compact_tool_surface(tools)
    }

    /// Rewrite compactable middle history with a model-generated summary.
    pub fn apply_summary(
        &self,
        messages: &mut Vec<aura_reasoner::Message>,
        summary: SummaryOutput,
    ) -> CompactionReport {
        apply_message_summary(messages, summary)
    }
}
