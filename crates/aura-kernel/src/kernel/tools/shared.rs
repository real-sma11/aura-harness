//! Helpers shared between the single-proposal and batch
//! tool-processing flows.
//!
//! Centralizing these here removes the duplication audit Phase 2a was
//! aimed at: building the `Proposal` from a `ToolProposal`, applying the
//! per-tool timeout, resolving live `ask` prompts, and turning a
//! `(verdict, optional execution outcome)` pair into the final
//! `ProcessResult` shape.

use crate::executor::{decode_tool_effect, ExecuteContext};
use crate::kernel::{
    ApprovalRequiredInfo, Kernel, PendingToolPrompt, ProcessResult, ToolApprovalRemember,
    ToolApprovalResponse, ToolDecision, ToolOutput,
};
use crate::policy::PolicyVerdict;
use aura_core::{
    Action, ActionKind, ContextHash, Decision, Effect, EffectKind, EffectStatus, Proposal,
    ProposalSet, RecordEntry, ToolCall, ToolProposal, ToolState, Transaction, UserDefaultMode,
    UserToolDefaults,
};
use std::collections::BTreeMap;
use std::time::Duration;

impl Kernel {
    /// Build the kernel-internal [`Proposal`] (a `Delegate` proposal whose
    /// payload is the serialized [`ToolCall`]) from a reasoner-emitted
    /// [`ToolProposal`]. Single and batch paths both go through this so
    /// the on-wire shape stays in lockstep.
    pub(super) fn kernel_proposal_from_tool_proposal(
        &self,
        proposal: &ToolProposal,
    ) -> Result<Proposal, crate::KernelError> {
        let tool_call = ToolCall::new(&proposal.tool, proposal.args.clone());
        let payload = serde_json::to_vec(&tool_call)
            .map_err(|e| crate::KernelError::Serialization(e.to_string()))?;
        Ok(Proposal::new(ActionKind::Delegate, payload))
    }

    /// Run a single tool action under `config.tool_timeout_ms` and convert a
    /// timeout into a failed `Effect`. Shared by `process_tool_proposal` and
    /// `process_tools` so both the single-proposal and batch paths apply the
    /// same per-tool budget (Invariant §1 / rules.md §6.2).
    pub(super) async fn execute_with_timeout(
        &self,
        ctx: &ExecuteContext,
        action: &Action,
    ) -> Effect {
        let tool_timeout = Duration::from_millis(self.config.tool_timeout_ms);
        match tokio::time::timeout(tool_timeout, self.executor.execute(ctx, action)).await {
            Ok(effect) => effect,
            Err(_) => {
                tracing::warn!(
                    action_id = %action.action_id,
                    timeout_ms = self.config.tool_timeout_ms,
                    "Tool execution timed out"
                );
                Effect::failed(
                    action.action_id,
                    EffectKind::Agreement,
                    format!("Tool timed out after {}ms", self.config.tool_timeout_ms),
                )
            }
        }
    }

    /// Resolve the additive tri-state `ask` layer.
    ///
    /// If the legacy verdict is a hard `Deny`, returns it unchanged.
    /// Otherwise consults the live-prompt bridge and may upgrade the
    /// verdict (e.g. an `Allow` becomes a `RequireApproval` or a
    /// user-confirmed `Deny`).
    pub(super) async fn resolve_live_ask_verdict(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        tool_use_id: String,
        legacy_verdict: PolicyVerdict,
    ) -> Result<PolicyVerdict, crate::KernelError> {
        if matches!(legacy_verdict, PolicyVerdict::Deny { .. }) {
            return Ok(legacy_verdict);
        }
        if matches!(
            legacy_verdict,
            PolicyVerdict::RequireApproval { prompt: None, .. }
        ) && self.policy.resolve_tool_state(tool_name) != ToolState::Ask
        {
            return Ok(legacy_verdict);
        }
        if legacy_verdict.is_allowed()
            || matches!(
                legacy_verdict,
                PolicyVerdict::RequireApproval { prompt: None, .. }
            )
        {
            let request_id = format!("{}:{tool_use_id}", self.agent_id.to_hex());
            let remember_options = self.live_prompt_remember_options();
            if let Some(verdict) = self.policy.live_tool_prompt_verdict(
                tool_name,
                args,
                self.agent_id,
                request_id,
                self.config.tool_approval_prompter.is_some(),
                remember_options,
            ) {
                return self.resolve_prompt_verdict(verdict).await;
            }
        }
        Ok(legacy_verdict)
    }

    async fn resolve_prompt_verdict(
        &self,
        verdict: PolicyVerdict,
    ) -> Result<PolicyVerdict, crate::KernelError> {
        let PolicyVerdict::RequireApproval {
            reason,
            prompt: Some(prompt),
        } = verdict
        else {
            return Ok(verdict);
        };

        let Some(prompter) = self.config.tool_approval_prompter.as_ref() else {
            return Ok(PolicyVerdict::Deny { reason });
        };

        let response = prompter.prompt(prompt.clone()).await.map_err(|e| {
            crate::KernelError::Internal(format!(
                "tool approval prompt failed for '{}': {e}",
                prompt.tool_name
            ))
        })?;

        self.apply_live_approval_response(&prompt, response)?;

        match response.decision {
            ToolState::Allow => Ok(PolicyVerdict::Allow),
            ToolState::Deny => Ok(PolicyVerdict::Deny {
                reason: format!("Tool '{}' was denied by the user", prompt.tool_name),
            }),
            ToolState::Ask => Ok(PolicyVerdict::RequireApproval {
                reason,
                prompt: Some(prompt),
            }),
        }
    }

    fn apply_live_approval_response(
        &self,
        prompt: &PendingToolPrompt,
        response: ToolApprovalResponse,
    ) -> Result<(), crate::KernelError> {
        match response.remember {
            ToolApprovalRemember::Once => {}
            ToolApprovalRemember::Session => self
                .policy
                .remember_tool_state_for_session(&prompt.tool_name, response.decision),
            ToolApprovalRemember::Forever => {
                let Some(user_id) = self.config.originating_user_id.as_deref() else {
                    return Err(crate::KernelError::Internal(format!(
                        "cannot remember tool '{}' forever without an originating user id",
                        prompt.tool_name
                    )));
                };
                let defaults = fold_tool_state_into_defaults(
                    &self.config.policy.user_default,
                    &prompt.tool_name,
                    response.decision,
                );
                self.store
                    .put_user_tool_defaults(user_id, &defaults)
                    .map_err(|e| {
                        crate::KernelError::Store(format!("put_user_tool_defaults: {e}"))
                    })?;
                self.policy
                    .remember_tool_state_for_session(&prompt.tool_name, response.decision);
            }
        }
        Ok(())
    }

    fn live_prompt_remember_options(&self) -> Vec<ToolApprovalRemember> {
        let mut options = vec![ToolApprovalRemember::Once, ToolApprovalRemember::Session];
        if self.config.originating_user_id.is_some() {
            options.push(ToolApprovalRemember::Forever);
        }
        options
    }
}

fn fold_tool_state_into_defaults(
    defaults: &UserToolDefaults,
    tool_name: &str,
    state: ToolState,
) -> UserToolDefaults {
    let (mut per_tool, fallback): (BTreeMap<String, ToolState>, ToolState) = match &defaults.mode {
        UserDefaultMode::FullAccess => (BTreeMap::new(), ToolState::Allow),
        UserDefaultMode::AutoReview => (BTreeMap::new(), ToolState::Ask),
        UserDefaultMode::DefaultPermissions { per_tool, fallback } => (per_tool.clone(), *fallback),
    };
    per_tool.insert(tool_name.to_string(), state);
    UserToolDefaults::default_permissions(per_tool, fallback)
}

/// Inputs for [`record_entry_for_tool_outcome`].
///
/// Bundled into a struct so the unified record-builder doesn't trip
/// clippy's `too_many_arguments` lint and so single/batch call sites
/// stay readable. Construct it inline at the call site.
pub(super) struct ToolOutcomeInputs<'a> {
    pub seq: u64,
    pub tx: Transaction,
    pub context_hash: ContextHash,
    pub kernel_proposal: Proposal,
    pub verdict: &'a PolicyVerdict,
    pub tool_use_id: String,
    pub tool_name: &'a str,
    pub executed: Option<(Action, Effect)>,
}

/// Build a [`RecordEntry`] and the surrounding [`ProcessResult`] from
/// the policy verdict and an optional execution outcome.
///
/// Contract: `inputs.executed.is_some()` iff `inputs.verdict.is_allowed()`.
/// Both single and batch callers honor that — `executed` is the
/// `(Action, Effect)` pair returned by `execute_with_timeout` for
/// approved proposals, and `None` for any verdict that didn't reach
/// execution.
pub(super) fn record_entry_for_tool_outcome(inputs: ToolOutcomeInputs<'_>) -> ProcessResult {
    let ToolOutcomeInputs {
        seq,
        tx,
        context_hash,
        kernel_proposal,
        verdict,
        tool_use_id,
        tool_name,
        executed,
    } = inputs;

    let mut proposals = ProposalSet::new();
    proposals.proposals.push(kernel_proposal);

    if let Some((action, effect)) = executed {
        let effect_failed = effect.status == EffectStatus::Failed;
        let decoded = decode_tool_effect(&effect);
        let had_failures = effect_failed || decoded.is_error;
        let line_diff = decoded.line_diff;
        let kind = decoded.kind;
        let output_content = decoded.content;

        let mut decision = Decision::new();
        decision.accept(action.action_id);

        let entry = RecordEntry::builder(seq, tx)
            .context_hash(context_hash)
            .proposals(proposals)
            .decision(decision)
            .actions(vec![action])
            .effects(vec![effect])
            .build();

        ProcessResult {
            entry,
            tool_output: Some(ToolOutput {
                tool_use_id,
                content: output_content,
                is_error: had_failures,
                kind,
                approval_required: None,
                line_diff,
            }),
            had_failures,
            runtime_capability_update: None,
            clear_runtime_capabilities: false,
            tool_decision: Some(ToolDecision::Allowed),
        }
    } else {
        let denial_reason = verdict
            .reason()
            .map_or_else(|| "Policy denied".to_string(), str::to_string);
        let prompt = match verdict {
            PolicyVerdict::RequireApproval { prompt, .. } => prompt.clone(),
            _ => None,
        };
        let needs_approval = matches!(verdict, PolicyVerdict::RequireApproval { .. });

        let mut decision = Decision::new();
        decision.reject(0, &denial_reason);

        let entry = RecordEntry::builder(seq, tx)
            .context_hash(context_hash)
            .proposals(proposals)
            .decision(decision)
            .build();

        let (approval_required, tool_decision) = if needs_approval {
            (
                Some(ApprovalRequiredInfo {
                    tool: tool_name.to_string(),
                    prompt: prompt.clone(),
                }),
                ToolDecision::NeedsApproval {
                    reason: denial_reason.clone(),
                    prompt,
                },
            )
        } else {
            (
                None,
                ToolDecision::Denied {
                    reason: denial_reason.clone(),
                },
            )
        };

        ProcessResult {
            entry,
            tool_output: Some(ToolOutput {
                tool_use_id,
                content: denial_reason,
                is_error: true,
                kind: aura_core::ToolResultKind::AgentError,
                approval_required,
                line_diff: None,
            }),
            had_failures: false,
            runtime_capability_update: None,
            clear_runtime_capabilities: false,
            tool_decision: Some(tool_decision),
        }
    }
}
