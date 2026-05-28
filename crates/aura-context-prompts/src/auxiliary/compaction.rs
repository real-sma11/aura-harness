//! Compaction summary LLM-call prompts.
//!
//! When the conversation history outgrows the model's context window
//! the agent loop fires an auxiliary LLM call that asks the model to
//! summarise the compactable middle of the transcript. This module
//! owns the **model-facing strings** the call uses (system prompt
//! constant + the user-prompt header template). The actual message
//! rendering (walking `aura_reasoner::Message` blocks and producing
//! the per-message text bodies) stays in `aura-agent` because it
//! requires the reasoner crate, which is a forbidden dep here (see
//! crate-level docs).
//!
//! ## Phase 2 deviation from the plan
//!
//! The plan instructed "auxiliary/compaction.rs — moved verbatim".
//! Doing so would force a `aura-reasoner` dependency on this crate
//! (the verbatim function takes `&[aura_reasoner::Message]` and walks
//! `ContentBlock` variants), which the boundary contract explicitly
//! forbids. The pragmatic split is:
//!
//! - The system prompt and the user-prompt header template + tail
//!   labels live here as `&'static str` constants / a small
//!   formatter taking only the numeric inputs.
//! - The Vec<Message>-walking message renderer lives in
//!   `aura-agent/src/agent_loop/` (the call site is already there)
//!   and concatenates the per-message bodies into the two `&str`
//!   blocks this module's formatter splices in.
//!
//! Net: every model-facing literal is still in `aura-prompts`; only
//! the reasoner-message walk that produces the per-message text
//! stays in `aura-agent`.

/// System prompt for the compaction summary LLM call.
///
/// Lifted verbatim from the pre-Phase-2 inline literal in
/// `aura_agent::agent_loop::AgentLoop::build_summary_request`.
pub const COMPACTION_SUMMARY_SYSTEM_PROMPT: &str =
    "Summarize compacted conversation history for a coding agent. \
     Preserve concrete decisions, files, tool outcomes, errors, and unresolved tasks. \
     Do not invent facts.";

/// Header literal preceding the rendered compactable-middle messages
/// in the compaction-summary user prompt. Pinned as a public const
/// so the `aura-agent` caller can splice it without re-implementing
/// the wording.
pub const COMPACTABLE_MIDDLE_HEADER: &str = "\n## Compactable Middle History\n";

/// Header literal preceding the rendered recent-tail messages. See
/// [`COMPACTABLE_MIDDLE_HEADER`].
pub const RECENT_TAIL_HEADER: &str = "\n## Recent Tail Kept Verbatim\n";

/// Rendered numeric / target inputs the user prompt header carries.
///
/// `aura-agent` populates this from the
/// `aura_compaction::SummaryInput` and threads it (plus the two
/// already-rendered message blocks) into [`build_compact_summary_user_prompt`].
#[derive(Debug, Clone, Copy)]
pub struct CompactSummaryHeader {
    /// `aura_compaction::SummaryInput::max_summary_chars` — the
    /// soft target the model is asked to hit.
    pub max_summary_chars: usize,
    /// `aura_compaction::SummaryInput::original_chars` — pre-local-
    /// compaction transcript size.
    pub original_chars: usize,
    /// `aura_compaction::SummaryInput::local_chars` — post-local-
    /// compaction transcript size.
    pub local_chars: usize,
    /// `aura_compaction::SummaryInput::target_total_chars` — desired
    /// total transcript size after the summary lands.
    pub target_total_chars: usize,
}

/// Render the compaction-summary user prompt.
///
/// `compactable_middle_block` and `recent_tail_block` are pre-rendered
/// by `aura-agent` (it walks the `Vec<aura_reasoner::Message>` slices
/// and produces the per-message text). Both arguments may be empty
/// strings when the corresponding section is empty.
#[must_use]
pub fn build_compact_summary_user_prompt(
    header: CompactSummaryHeader,
    compactable_middle_block: &str,
    recent_tail_block: &str,
) -> String {
    let CompactSummaryHeader {
        max_summary_chars,
        original_chars,
        local_chars,
        target_total_chars,
    } = header;
    let mut prompt = format!(
        "Summarize the compactable middle history below into no more than about {max_summary_chars} characters.\n\
         The live transcript was {original_chars} chars before local compaction and {local_chars} chars after local compaction.\n\
         Target total transcript chars after summary: {target_total_chars}.\n\
         Keep exact file paths, tool names, important outputs, decisions, and unresolved errors.\n",
    );
    prompt.push_str(COMPACTABLE_MIDDLE_HEADER);
    prompt.push_str(compactable_middle_block);
    prompt.push_str(RECENT_TAIL_HEADER);
    prompt.push_str(recent_tail_block);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_prompt_includes_targets_and_sections() {
        let prompt = build_compact_summary_user_prompt(
            CompactSummaryHeader {
                max_summary_chars: 1_000,
                original_chars: 5_000,
                local_chars: 4_000,
                target_total_chars: 2_000,
            },
            "<middle body>",
            "<tail body>",
        );
        assert!(prompt.contains("Summarize the compactable middle history below"));
        assert!(prompt.contains("no more than about 1000 characters"));
        assert!(prompt.contains("Target total transcript chars after summary: 2000"));
        assert!(prompt.contains("## Compactable Middle History"));
        assert!(prompt.contains("## Recent Tail Kept Verbatim"));
        assert!(prompt.contains("<middle body>"));
        assert!(prompt.contains("<tail body>"));
    }

    #[test]
    fn system_prompt_is_stable() {
        assert!(COMPACTION_SUMMARY_SYSTEM_PROMPT.starts_with("Summarize compacted conversation"));
        assert!(COMPACTION_SUMMARY_SYSTEM_PROMPT.contains("Do not invent facts"));
    }
}
