//! Batch tool-proposal processing flow.
//!
//! Used when the runtime hands the kernel multiple [`ToolProposal`]s in
//! a single turn. Approved proposals execute concurrently via
//! `futures_util::join_all`; all entries (approved + denied) are then
//! written through `append_entries_batch` against a single contiguous
//! sequence range so the batch is atomic.

use super::shared::{record_entry_for_tool_outcome, ToolOutcomeInputs};
use crate::context::hash_tx_with_window;
use crate::executor::ExecuteContext;
use crate::kernel::{Kernel, ProcessResult};
use crate::policy::PolicyVerdict;
use aura_core::{Action, ActionId, ActionKind, Effect, Proposal, ToolProposal, Transaction};

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

    let mut exec_contexts: Vec<ExecuteContext> = Vec::new();
    let mut exec_actions: Vec<Action> = Vec::new();

    for (i, _proposal) in tool_proposals.iter().enumerate() {
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
        let ctx = ExecuteContext::new(kernel.agent_id, action_id, workspace.clone());

        exec_contexts.push(ctx);
        exec_actions.push(action);
    }

    let exec_futures = exec_contexts
        .iter()
        .zip(exec_actions.iter())
        .map(|(ctx, action)| kernel.execute_with_timeout(ctx, action));

    let effects: Vec<Effect> = futures_util::future::join_all(exec_futures).await;

    let total = tool_proposals.len();
    let base_seq = kernel.reserve_seq_range(total)?;

    let mut results = Vec::with_capacity(total);
    let mut entries = Vec::with_capacity(total);
    let mut approved_idx = 0;

    for (i, proposal) in tool_proposals.into_iter().enumerate() {
        let seq = base_seq + i as u64;
        let tx = Transaction::tool_proposal(kernel.agent_id, &proposal)
            .map_err(|e| crate::KernelError::Serialization(e.to_string()))?;

        let window = kernel.load_window(seq)?;
        let context_hash = hash_tx_with_window(&tx, &window)?;
        let tool_use_id = proposal.tool_use_id.clone();

        let verdict = &verdicts[i];
        let executed = if verdict.is_allowed() {
            let action = exec_actions[approved_idx].clone();
            let effect = effects[approved_idx].clone();
            approved_idx += 1;
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
