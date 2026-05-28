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
//! 4. **Phase 10 carve-out 5a**: fire `PreToolUse` BEFORE dispatch
//!    when a [`crate::kernel::KernelConfig::plugin_hooks`] is
//!    attached. On [`aura_plugin_hooks::HookOutcome::Block`] the
//!    executor is skipped, a parallel `tool_call_blocked_by_hook`
//!    System audit row is written, and a synthetic
//!    [`aura_core::Effect::failed`] is surfaced so the agent loop
//!    sees a clean rejection.
//! 5. If approved (and not blocked), execute under the per-tool
//!    timeout.
//! 6. Build the `RecordEntry` + `ProcessResult` via
//!    [`super::shared::record_entry_for_tool_outcome`].

use super::shared::{record_entry_for_tool_outcome, ToolOutcomeInputs};
use crate::executor::ExecuteContext;
use crate::kernel::{Kernel, ProcessResult};
use aura_core::{
    Action, ActionId, ActionKind, ContextHash, Effect, EffectKind, ToolProposal, Transaction,
};
use bytes::Bytes;

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

        // Phase 10 carve-out 5a: fire `PreToolUse` BEFORE dispatch.
        // On `HookOutcome::Block` we never invoke the executor — a
        // synthetic failed [`Effect`] carrying the
        // `tool_call_blocked_by_hook` JSON-discriminated payload
        // takes the executor's place. The discriminator surfaces
        // the schema-v2 [`aura_store_record::RecordKind::ToolCallBlockedByHook`]
        // taxonomy at the effect-payload level so consumers can
        // distinguish a hook block from a normal executor failure.
        let blocked_effect = if let Some(host) = kernel.config.plugin_hooks.as_ref() {
            fire_pre_tool_use_and_maybe_block(host, &proposal, action_id)?
        } else {
            None
        };

        if let Some(effect) = blocked_effect {
            Some((action, effect))
        } else {
            let workspace = kernel.agent_workspace();
            tokio::fs::create_dir_all(&workspace)
                .await
                .map_err(|e| crate::KernelError::Internal(format!("create workspace: {e}")))?;
            let ctx = ExecuteContext::new(kernel.agent_id, action_id, workspace);
            let effect = kernel.execute_with_timeout(&ctx, &action).await;
            Some((action, effect))
        }
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

/// Fire `PreToolUse` and translate the aggregate outcome.
///
/// Returns `Ok(None)` for any non-blocking outcome (`Continue`,
/// `TimedOut`, `Approve`, `Deny`, `Replace`) — the dispatcher
/// proceeds normally in those cases.
///
/// Returns `Ok(Some(effect))` when a handler returned
/// [`aura_plugin_hooks::HookOutcome::Block`]. The synthetic
/// [`Effect`] carries a JSON-discriminated payload of the form
/// `{"kind": "tool_call_blocked_by_hook", "tool_name": ...,
/// "tool_use_id": ..., "reason": ...}` and
/// [`aura_core::EffectStatus::Failed`] as its status. The
/// discriminator surfaces the Phase 10 schema-v2
/// [`aura_store_record::RecordKind::ToolCallBlockedByHook`]
/// taxonomy at the effect-payload level so an auditor can
/// distinguish a hook block from a normal executor failure while
/// the surrounding `RecordEntry` keeps a single deterministic
/// sequence number (no parallel `write_system_record` write).
fn fire_pre_tool_use_and_maybe_block(
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
            "PreToolUse hook returned Block; dispatch aborted (Phase 10 5a)"
        );
        let effect = Effect::failed(action_id, EffectKind::Agreement, Bytes::from(bytes));
        return Ok(Some(effect));
    }
    Ok(None)
}
