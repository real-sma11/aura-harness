//! Rebuild a [`ModelResponse`] from the per-block chunks the pump
//! observed during a streaming sampling request.
//!
//! The pump consumes `OutputItemDone(Message | Thinking | ToolUse)`
//! events one at a time; the downstream
//! `iteration::accumulate_response` and the unified
//! [`super::super::tool_pipeline::dispatch`] tail (Phase 4) both
//! expect a single [`ModelResponse`]. This module performs the
//! reassembly and is the home of the
//! `Completed.end_turn` → [`StopReason`] mapping.

use aura_reasoner::{ContentBlock, Message, ModelResponse, ProviderTrace, Role, StopReason, Usage};

use crate::types::ToolCallInfo;

/// Reassemble a [`ModelResponse`] from the per-block chunks observed
/// during streaming.
///
/// # `Completed.end_turn` → [`StopReason`] mapping (Phase 3 fix)
///
/// The pump observes [`aura_reasoner::ResponseEvent::Completed`]'s
/// `end_turn: Option<bool>`, which is the lossy reduction applied at
/// `response_stream_from_response`:
///
/// ```text
/// StopReason::EndTurn | StopReason::StopSequence => Some(true)
/// StopReason::ToolUse | StopReason::MaxTokens    => Some(false)
/// (none reported)                                  => None
/// ```
///
/// (Source: `aura-reasoner/src/response_stream.rs:191-194`.) The
/// `aura-reasoner/src/anthropic/sse/event.rs:58-63` adapter that
/// produces the underlying `MessageDelta.stop_reason` already
/// preserves `ToolUse` / `MaxTokens` / `StopSequence` / `EndTurn`
/// distinctly — the fidelity is lost only at the `Completed`
/// boundary because `Option<bool>` cannot represent four states.
///
/// Pre-Phase-3 the pump mapped `Some(false)` with no observed tool
/// calls directly to [`StopReason::MaxTokens`]. That was lossy in
/// the wrong direction:
///
/// * Anthropic's `stop_reason="tool_use"` is the *normal* "more work
///   needed" signal; `max_tokens` is the truncation tail.
/// * Treating `Some(false)` as `MaxTokens` falsely tagged ordinary
///   terminations as truncation-driven, which made downstream
///   diagnostics misleading (the `iteration::handle_max_tokens`
///   warn-and-restore-budget path would tracelog "MaxTokens with
///   pending tool_use blocks" even when no truncation occurred).
/// * Both `StopReason::ToolUse` and `StopReason::MaxTokens` collapse
///   to "break loop" through the unified
///   [`super::super::tool_pipeline::dispatch`] entry point when the
///   accumulated content has no tool-use / pending-tool blocks
///   (see `tool_pipeline::process_tool_results` /
///   `iteration::handle_max_tokens`), so the observable downstream
///   behaviour is identical — only the diagnostic intent differs.
///
/// Phase 3 maps `Some(false)` with no observed tool calls to
/// [`StopReason::ToolUse`] instead. This matches the Anthropic
/// contract default for "more work" and stops falsely flagging
/// stops as truncation-driven. The arm with observed tool calls
/// (always wins) and the `Some(true)` / `None` end-turn arm are
/// unchanged.
pub(super) fn synthesize_response(
    text_chunks: &[String],
    thinking_chunks: &[(String, Option<String>)],
    tool_calls: &[ToolCallInfo],
    end_turn: Option<bool>,
    usage: &Usage,
    model_name: &str,
) -> ModelResponse {
    let mut content: Vec<ContentBlock> = Vec::new();
    for (thinking, signature) in thinking_chunks {
        content.push(ContentBlock::Thinking {
            thinking: thinking.clone(),
            signature: signature.clone(),
        });
    }
    for text in text_chunks {
        content.push(ContentBlock::Text { text: text.clone() });
    }
    for call in tool_calls {
        content.push(ContentBlock::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        });
    }

    let stop_reason = derive_stop_reason(tool_calls, end_turn);

    ModelResponse::new(
        stop_reason,
        Message::new(Role::Assistant, content),
        usage.clone(),
        ProviderTrace::new(model_name, 0),
    )
}

/// Derive [`StopReason`] from the pump-observed end_turn bit + the
/// presence of any actual tool calls. See the module-level docs for
/// the contract.
fn derive_stop_reason(tool_calls: &[ToolCallInfo], end_turn: Option<bool>) -> StopReason {
    if !tool_calls.is_empty() {
        // Tool calls were emitted: this is unambiguously
        // `StopReason::ToolUse` regardless of `end_turn` (Anthropic's
        // `stop_reason="tool_use"` is the canonical signal).
        return StopReason::ToolUse;
    }
    match end_turn {
        // Provider explicitly signalled "more work" without emitting
        // tool calls. Per the docstring above this is the
        // Anthropic-contract default for `ToolUse`, not `MaxTokens`.
        // Both variants collapse to "break loop" downstream when no
        // tool-use / pending-tool blocks were accumulated, so the
        // observable behaviour is unchanged; the change here is in
        // diagnostic intent.
        Some(false) => StopReason::ToolUse,
        // `Some(true)`: model said "I'm done". `None`: provider did
        // not report `end_turn` (legacy / mock paths); default to
        // EndTurn so the loop terminates rather than spinning on
        // an undefined signal.
        Some(true) | None => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_call(id: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn tool_calls_present_yields_tool_use_regardless_of_end_turn() {
        let calls = vec![mk_call("toolu_a")];
        assert_eq!(derive_stop_reason(&calls, Some(true)), StopReason::ToolUse);
        assert_eq!(derive_stop_reason(&calls, Some(false)), StopReason::ToolUse);
        assert_eq!(derive_stop_reason(&calls, None), StopReason::ToolUse);
    }

    /// Phase 3 fix: `Some(false)` with no observed tool calls now
    /// surfaces as `ToolUse` (Anthropic-contract default for "more
    /// work"), not `MaxTokens`. Pre-Phase-3 this falsely tagged
    /// ordinary terminations as truncation-driven.
    #[test]
    fn end_turn_false_with_no_tool_calls_yields_tool_use_not_max_tokens() {
        let calls: Vec<ToolCallInfo> = Vec::new();
        let derived = derive_stop_reason(&calls, Some(false));
        assert_eq!(
            derived,
            StopReason::ToolUse,
            "end_turn=Some(false) with no tool calls must map to ToolUse per the Anthropic \
             stop_reason contract; mapping to MaxTokens (the pre-Phase-3 behaviour) falsely \
             tags every empty-content stop as truncation-driven"
        );
    }

    #[test]
    fn end_turn_true_yields_end_turn() {
        let calls: Vec<ToolCallInfo> = Vec::new();
        assert_eq!(derive_stop_reason(&calls, Some(true)), StopReason::EndTurn);
    }

    #[test]
    fn end_turn_none_yields_end_turn() {
        let calls: Vec<ToolCallInfo> = Vec::new();
        assert_eq!(derive_stop_reason(&calls, None), StopReason::EndTurn);
    }
}
