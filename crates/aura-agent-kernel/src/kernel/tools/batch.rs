//! Batch tool-proposal processing flow.
//!
//! Used when the runtime hands the kernel multiple [`ToolProposal`]s in
//! a single turn. Approved proposals execute concurrently via
//! `futures_util::join_all`; all entries (approved + denied) are then
//! written through `append_entries_batch` against a single contiguous
//! sequence range so the batch is atomic.

use super::shared::{record_entry_for_tool_outcome, ToolOutcomeInputs};
use crate::context::hash_tx_with_window;
use aura_exec_traits::ExecuteContext;
use crate::kernel::{Kernel, ProcessResult};
use crate::policy::PolicyVerdict;
use aura_core::{
    Action, ActionId, ActionKind, Effect, EffectKind, Proposal, ToolProposal, Transaction,
};
use bytes::Bytes;

pub(super) async fn process_many(
    kernel: &Kernel,
    tool_proposals: Vec<ToolProposal>,
) -> Result<Vec<ProcessResult>, crate::KernelError> {
    if tool_proposals.is_empty() {
        return Ok(vec![]);
    }

    let mut kernel_proposals: Vec<Proposal> = Vec::with_capacity(tool_proposals.len());
    let mut verdicts: Vec<PolicyVerdict> = Vec::with_capacity(tool_proposals.len());
    let runtime_capabilities = kernel.load_runtime_capabilities()?;

    for proposal in &tool_proposals {
        let kernel_proposal = kernel.kernel_proposal_from_tool_proposal(proposal)?;
        let verdict = kernel.policy.check_with_runtime_capabilities_verdict(
            &kernel_proposal,
            runtime_capabilities.as_ref(),
        );
        let verdict = kernel
            .resolve_live_ask_verdict(
                &proposal.tool,
                &proposal.args,
                proposal.tool_use_id.clone(),
                verdict,
            )
            .await?;
        verdicts.push(verdict);
        kernel_proposals.push(kernel_proposal);
    }

    let workspace = kernel.agent_workspace();
    tokio::fs::create_dir_all(&workspace)
        .await
        .map_err(|e| crate::KernelError::Internal(format!("create workspace: {e}")))?;

    // Phase 10 carve-out 5a: fire `PreToolUse` per-proposal BEFORE
    // executor dispatch. Any handler returning
    // `HookOutcome::Block` causes us to skip the executor for
    // that slot and substitute a synthetic failed
    // [`aura_core::Effect`] carrying the block reason; a parallel
    // `tool_call_blocked_by_hook` System audit row is written by
    // [`fire_pre_tool_use_for_batch`] so the audit log can
    // distinguish the block from a normal executor failure.
    //
    // We pre-allocate per-proposal slots so the post-execution
    // pairing loop walks the same `(proposal, action, effect)`
    // tuples without a side index map.
    let total = tool_proposals.len();
    let mut per_slot_action: Vec<Option<Action>> = vec![None; total];
    let mut per_slot_effect: Vec<Option<Effect>> = vec![None; total];
    let mut dispatch_indices: Vec<usize> = Vec::new();
    let mut dispatch_contexts: Vec<ExecuteContext> = Vec::new();
    let mut dispatch_actions: Vec<Action> = Vec::new();

    for (i, proposal) in tool_proposals.iter().enumerate() {
        let verdict = &verdicts[i];
        if !verdict.is_allowed() {
            continue;
        }
        let action_id = ActionId::generate();
        let action = Action::new(
            action_id,
            ActionKind::Delegate,
            kernel_proposals[i].payload.clone(),
        );

        if let Some(host) = kernel.config.plugin_hooks.as_ref() {
            if let Some(effect) = fire_pre_tool_use_for_batch(host, proposal, action_id)? {
                per_slot_action[i] = Some(action);
                per_slot_effect[i] = Some(effect);
                continue;
            }
        }

        per_slot_action[i] = Some(action.clone());
        let ctx = ExecuteContext::new(kernel.agent_id, action_id, workspace.clone());
        dispatch_indices.push(i);
        dispatch_contexts.push(ctx);
        dispatch_actions.push(action);
    }

    let exec_futures = dispatch_contexts
        .iter()
        .zip(dispatch_actions.iter())
        .map(|(ctx, action)| kernel.execute_with_timeout(ctx, action));
    let dispatched_effects: Vec<Effect> = futures_util::future::join_all(exec_futures).await;

    for (slot, effect) in dispatch_indices.iter().zip(dispatched_effects.into_iter()) {
        per_slot_effect[*slot] = Some(effect);
    }

    let base_seq = kernel.reserve_seq_range(total)?;

    let mut results = Vec::with_capacity(total);
    let mut entries = Vec::with_capacity(total);

    for (i, proposal) in tool_proposals.into_iter().enumerate() {
        let seq = base_seq + i as u64;
        let tx = Transaction::tool_proposal(kernel.agent_id, &proposal)
            .map_err(|e| crate::KernelError::Serialization(e.to_string()))?;

        let window = kernel.load_window(seq)?;
        let context_hash = hash_tx_with_window(&tx, &window)?;
        let tool_use_id = proposal.tool_use_id.clone();

        let verdict = &verdicts[i];
        let executed = if verdict.is_allowed() {
            let action = per_slot_action[i]
                .take()
                .expect("allowed verdict must have produced an action");
            let effect = per_slot_effect[i]
                .take()
                .expect("allowed verdict must have produced an effect");
            Some((action, effect))
        } else {
            None
        };

        let result = record_entry_for_tool_outcome(ToolOutcomeInputs {
            seq,
            tx,
            context_hash,
            kernel_proposal: kernel_proposals[i].clone(),
            verdict,
            tool_use_id,
            tool_name: &proposal.tool,
            executed,
            lite_threshold: kernel.lite_payload_threshold(),
        });

        entries.push(result.entry.clone());
        results.push(result);
    }

    kernel
        .store
        .append_entries_batch(kernel.agent_id, base_seq, &entries)
        .map_err(|e| crate::KernelError::Store(format!("append_entries_batch: {e}")))?;

    Ok(results)
}

/// Phase 10 carve-out 5a — `PreToolUse` pre-dispatch firing for the
/// batch flow. Mirrors `single::fire_pre_tool_use_and_maybe_block`:
/// the synthetic [`Effect`] carries a `tool_call_blocked_by_hook`
/// JSON-discriminated payload so a single deterministic sequence
/// number is consumed per blocked slot (no parallel
/// `write_system_record` write that would race the batch's
/// `reserve_seq_range`).
fn fire_pre_tool_use_for_batch(
    host: &aura_plugin_hooks::PluginHookHost,
    proposal: &ToolProposal,
    action_id: ActionId,
) -> Result<Option<Effect>, crate::KernelError> {
    if host.is_empty(aura_plugin_hooks::HookEvent::PreToolUse) {
        return Ok(None);
    }
    let args_text = serde_json::to_string(&proposal.args).unwrap_or_default();
    let outcome = host.fire_pre_tool_use(&proposal.tool, &args_text, &proposal.tool_use_id);
    if let aura_plugin_hooks::HookOutcome::Block { reason } = outcome.decision {
        let payload = serde_json::json!({
            "kind": "tool_call_blocked_by_hook",
            "tool_name": proposal.tool,
            "tool_use_id": proposal.tool_use_id,
            "reason": reason,
        });
        let bytes = serde_json::to_vec(&payload)
            .map_err(|e| crate::KernelError::Serialization(format!("block payload: {e}")))?;
        tracing::info!(
            tool_name = %proposal.tool,
            tool_use_id = %proposal.tool_use_id,
            "PreToolUse hook returned Block; dispatch aborted (Phase 10 5a, batch path)"
        );
        let effect = Effect::failed(action_id, EffectKind::Agreement, Bytes::from(bytes));
        return Ok(Some(effect));
    }
    Ok(None)
}
