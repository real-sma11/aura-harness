//! Single-proposal tool processing flow.
//!
//! Used when the runtime hands the kernel exactly one [`ToolProposal`]
//! (typically because the model emitted a single tool-use block in a
//! turn). The flow:
//!
//! 1. Build the kernel-internal `Delegate` proposal from the
//!    `ToolProposal` payload.
//! 2. Run the policy + runtime-capability gate.
//! 3. Resolve any live `ask` prompt.
//! 4. If approved, execute under the per-tool timeout.
//! 5. Build the `RecordEntry` + `ProcessResult` via
//!    [`super::shared::record_entry_for_tool_outcome`].

use super::shared::{record_entry_for_tool_outcome, ToolOutcomeInputs};
use crate::executor::ExecuteContext;
use crate::kernel::{Kernel, ProcessResult};
use aura_core::{Action, ActionId, ActionKind, ContextHash, ToolProposal, Transaction};

pub(super) async fn process_one(
    kernel: &Kernel,
    tx: &Transaction,
    seq: u64,
    context_hash: ContextHash,
) -> Result<ProcessResult, crate::KernelError> {
    let proposal: ToolProposal = serde_json::from_slice(&tx.payload)
        .map_err(|e| crate::KernelError::Serialization(format!("deserialize ToolProposal: {e}")))?;

    let tool_use_id = proposal.tool_use_id.clone();
    let tool_name = proposal.tool.clone();

    let kernel_proposal = kernel.kernel_proposal_from_tool_proposal(&proposal)?;

    let runtime_capabilities = kernel.load_runtime_capabilities()?;
    let verdict = kernel
        .policy
        .check_with_runtime_capabilities_verdict(&kernel_proposal, runtime_capabilities.as_ref());

    let verdict = kernel
        .resolve_live_ask_verdict(&tool_name, &proposal.args, tool_use_id.clone(), verdict)
        .await?;

    let executed = if verdict.is_allowed() {
        let action_id = ActionId::generate();
        let action = Action::new(
            action_id,
            ActionKind::Delegate,
            kernel_proposal.payload.clone(),
        );

        let workspace = kernel.agent_workspace();
        tokio::fs::create_dir_all(&workspace)
            .await
            .map_err(|e| crate::KernelError::Internal(format!("create workspace: {e}")))?;
        let ctx = ExecuteContext::new(kernel.agent_id, action_id, workspace);
        let effect = kernel.execute_with_timeout(&ctx, &action).await;
        Some((action, effect))
    } else {
        None
    };

    Ok(record_entry_for_tool_outcome(ToolOutcomeInputs {
        seq,
        tx: tx.clone(),
        context_hash,
        kernel_proposal,
        verdict: &verdict,
        tool_use_id,
        tool_name: &tool_name,
        executed,
        lite_threshold: kernel.lite_payload_threshold(),
    }))
}
