//! Summary-compaction tail.
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split. Phase 4 removed the per-`AgentLoop` `dispatch_stop_reason`
//! method that previously lived here — the unified
//! [`super::tool_pipeline::dispatch`] entry point now owns
//! stop-reason routing for both transports. What remains:
//!
//! - [`AgentLoop::apply_summary_compaction`] — the auxiliary
//!   model-call that turns a [`aura_context_compaction::SummaryInput`] into a
//!   replacement transcript prefix.
//! - [`AgentLoop::build_summary_request`] — the
//!   [`ModelRequest`] builder for the summary call (kept private to
//!   the loop). Renders the user prompt via
//!   [`aura_context_prompts::auxiliary::compaction`] per the Phase 2 prompts
//!   boundary contract.
//!
//! Phase 7 deleted the pre-pump `retry_after_context_overflow`
//! ladder along with the `BufferedTransport` it served. The pump
//! transport surfaces `PromptTooLong` as a fatal model error today;
//! reintroducing pump-level overflow recovery should rebuild the
//! ladder against the active transport rather than resurrect the
//! buffered helper.

use aura_config::CHARS_PER_TOKEN;
use aura_context_compaction as compaction;
use aura_model_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelRequestKind, ThinkingEffort,
    ToolChoice, ToolDefinition,
};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::events::AgentLoopEvent;

use super::config::parse_cache_retention;
#[cfg(test)]
use super::config::AgentLoopConfig;
use super::run::is_cancelled;
use super::state::LoopState;
use super::{context, iteration, AgentLoop};

impl AgentLoop {
    /// Drive the auxiliary "compaction-summary" model call when the
    /// inline compactor reports that local rules alone won't fit the
    /// budget. Mutates `state` in place by rewriting the message
    /// history with the model-rendered summary.
    pub(super) async fn apply_summary_compaction(
        &self,
        provider: &dyn ModelProvider,
        tools: &[ToolDefinition],
        _event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
        state: &mut LoopState,
        input: compaction::SummaryInput,
    ) {
        if is_cancelled(cancellation_token) {
            return;
        }

        let request = match self.build_summary_request(&input) {
            Ok(request) => request,
            Err(e) => {
                warn!(error = %e, "failed to build compaction summary request");
                return;
            }
        };

        let response = match self
            .call_model(provider, request, None, cancellation_token)
            .await
        {
            Ok(response) => response,
            Err(e) => {
                warn!(
                    error = %summary_error_for_log(&e),
                    "compaction summary generation failed; continuing with local compaction"
                );
                return;
            }
        };

        let summary_text = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if summary_text.trim().is_empty() {
            warn!(
                "compaction summary generation returned no text; continuing with local compaction"
            );
            return;
        }

        context::apply_summary_output(
            &self.config,
            state,
            tools,
            compaction::SummaryOutput::Messages {
                text: summary_text,
                compact_start: input.compact_start,
                compact_end: input.compact_end,
            },
        );
    }

    fn build_summary_request(
        &self,
        input: &compaction::SummaryInput,
    ) -> Result<ModelRequest, crate::AgentError> {
        let prompt = super::compaction_summary::render_user_prompt(input);
        let max_tokens = (input.max_summary_chars / CHARS_PER_TOKEN)
            .clamp(256, 4_096)
            .try_into()
            .unwrap_or(4_096);

        ModelRequest::builder(
            &self.config.model,
            aura_context_prompts::auxiliary::compaction::COMPACTION_SUMMARY_SYSTEM_PROMPT,
        )
        .messages(vec![Message::user(prompt)])
        .tools(Vec::new())
        .tool_choice(ToolChoice::None)
        .max_tokens(max_tokens)
        // Force extended thinking *off* on the compaction-summary
        // call. The earlier `Medium` pin (added so the console row
        // didn't render `thinking off` for parity with the dev-loop
        // policy) interacts badly with the tight `max_tokens` clamp
        // above (256..=4_096): on Claude 4.x with `adaptive` thinking,
        // the model consumes the entire budget on a thinking block
        // and returns an empty text body. `apply_summary_compaction`
        // then hits its empty-text early-return, the messages are
        // never reduced, and `compact_if_needed` re-fires `NeedsSummary`
        // on the next iteration — doubling the outbound API call rate
        // for the rest of the task while doing no actual compaction.
        // The summary call is mechanical (rewrite N kB of transcript
        // into ~M chars); thinking-off is the right policy and the
        // console renders it as such.
        .thinking_effort(Some(ThinkingEffort::Off))
        .auth_token(self.config.auth_token.clone())
        .upstream_provider_family(self.config.upstream_provider_family.clone())
        .aura_project_id(self.config.aura_project_id.clone())
        .aura_agent_id(self.config.aura_agent_id.clone())
        .aura_session_id(self.config.aura_session_id.clone())
        .aura_org_id(self.config.aura_org_id.clone())
        .prompt_cache_key(self.config.prompt_cache_key.clone())
        .prompt_cache_retention(parse_cache_retention(
            self.config.prompt_cache_retention.as_deref(),
        ))
        .request_kind(ModelRequestKind::Auxiliary)
        .try_build()
        .map_err(crate::AgentError::from)
    }
}

fn summary_error_for_log(error: &iteration::LlmCallError) -> &'static str {
    match error {
        iteration::LlmCallError::InsufficientCredits(_) => "insufficient_credits",
        iteration::LlmCallError::PromptTooLong(_) => "prompt_too_long",
        iteration::LlmCallError::RateLimited(_) => "rate_limited",
        iteration::LlmCallError::Fatal(_) => "fatal",
    }
}

#[cfg(test)]
mod summary_request_tests {
    use super::*;
    use compaction::SummaryInput;

    fn sample_summary_input() -> SummaryInput {
        SummaryInput {
            compact_start: 0,
            compact_end: 1,
            compactable_messages: vec![Message::user("first")],
            recent_tail: vec![Message::assistant("latest")],
            original_chars: 5_000,
            local_chars: 4_000,
            target_total_chars: 2_000,
            max_summary_chars: 1_000,
        }
    }

    /// Regression: the auxiliary compaction-summary call must ship
    /// `thinking_effort = Off`. Setting it to `Medium` (which a prior
    /// WIP change tried, for parity with the dev-loop thinking pin)
    /// interacts badly with the tight `max_tokens` clamp (256..=4_096):
    /// Claude 4.x with adaptive thinking burns the entire budget on a
    /// thinking block and returns an empty text body, which makes
    /// `apply_summary_compaction` early-return without ever shrinking
    /// the transcript. `compact_if_needed` then re-fires `NeedsSummary`
    /// on every subsequent iteration, doubling the outbound API call
    /// rate for the rest of the task while doing no actual compaction.
    /// The companion fix in `effective_compaction_request_kind`
    /// addresses *why* `NeedsSummary` was firing every turn, but the
    /// summary call itself must also be able to produce real output.
    #[test]
    fn build_summary_request_disables_thinking() {
        let config = AgentLoopConfig::for_agent("aura-claude-opus-4-7");
        let agent = AgentLoop::new(config);
        let input = sample_summary_input();

        let request = agent
            .build_summary_request(&input)
            .expect("summary request builder must succeed for valid inputs");

        assert_eq!(
            request.thinking_effort,
            Some(ThinkingEffort::Off),
            "compaction-summary call must NOT enable extended thinking; the \
             tight max_tokens clamp would otherwise starve the actual summary \
             output (see comment on the .thinking_effort(..) line)"
        );
    }

    /// The summary call is single-shot: one user message, zero tools.
    /// Pins the request shape so an accidental tool-list bleed-through
    /// or extra messages from the live transcript would fail loudly.
    #[test]
    fn build_summary_request_ships_clean_single_shot_payload() {
        let config = AgentLoopConfig::for_agent("aura-claude-opus-4-7");
        let agent = AgentLoop::new(config);
        let input = sample_summary_input();

        let request = agent.build_summary_request(&input).unwrap();

        assert_eq!(request.messages.len(), 1, "exactly one user message");
        assert_eq!(request.tools.len(), 0, "no tools attached");
        assert!(matches!(request.tool_choice, ToolChoice::None));
        assert_eq!(request.metadata.kind, Some(ModelRequestKind::Auxiliary));
    }
}
