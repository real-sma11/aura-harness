//! Top-level transaction processing for the kernel.
//!
//! `process_direct` and `process_dequeued` bracket the `process_tx`
//! dispatcher, feeding the resulting `ProcessResult` to the matching store
//! append variant (direct / dequeued / capability-aware). `process_tx`
//! itself routes each transaction to the appropriate handler based on its
//! [`TransactionType`] and owns the `System`-transaction capability-install
//! decode path.

use super::{Kernel, ProcessResult};
use crate::context::hash_tx_with_window;
use aura_core::{RecordEntry, RuntimeCapabilityInstall, Transaction, TransactionType};
use aura_store::DequeueToken;

impl Kernel {
    /// Process a transaction from a direct (non-inbox) source.
    ///
    /// # Errors
    /// Returns error if processing or storage fails.
    pub async fn process_direct(
        &self,
        tx: Transaction,
    ) -> Result<ProcessResult, crate::KernelError> {
        let seq = self.next_seq()?;
        let result = self.process_tx(&tx, seq).await?;
        self.store
            .append_entry_direct_with_runtime_capabilities(
                self.agent_id,
                seq,
                &result.entry,
                result.runtime_capability_update.as_ref(),
                result.clear_runtime_capabilities,
            )
            .map_err(|e| {
                crate::KernelError::Store(format!(
                    "append_entry_direct_with_runtime_capabilities: {e}"
                ))
            })?;
        Ok(result)
    }

    /// Process a transaction dequeued from the inbox.
    ///
    /// # Errors
    /// Returns error if processing or storage fails.
    pub async fn process_dequeued(
        &self,
        tx: Transaction,
        token: DequeueToken,
    ) -> Result<ProcessResult, crate::KernelError> {
        let seq = self.next_seq()?;
        let result = self.process_tx(&tx, seq).await?;
        self.store
            .append_entry_dequeued_with_runtime_capabilities(
                self.agent_id,
                seq,
                &result.entry,
                token,
                result.runtime_capability_update.as_ref(),
                result.clear_runtime_capabilities,
            )
            .map_err(|e| {
                crate::KernelError::Store(format!(
                    "append_entry_dequeued_with_runtime_capabilities: {e}"
                ))
            })?;
        Ok(result)
    }

    pub(super) async fn process_tx(
        &self,
        tx: &Transaction,
        seq: u64,
    ) -> Result<ProcessResult, crate::KernelError> {
        let window = self.load_window(seq)?;
        let context_hash = hash_tx_with_window(tx, &window)?;

        match tx.tx_type {
            TransactionType::ToolProposal => {
                self.process_tool_proposal(tx, seq, context_hash).await
            }
            TransactionType::SessionStart => {
                self.policy.clear_session_approvals();
                let entry = RecordEntry::builder(seq, tx.clone())
                    .context_hash(context_hash)
                    .build();
                Ok(ProcessResult {
                    entry,
                    tool_output: None,
                    had_failures: false,
                    runtime_capability_update: None,
                    clear_runtime_capabilities: true,
                    tool_decision: None,
                })
            }
            TransactionType::System => {
                let runtime_capability_update = Self::runtime_capability_update_from_tx(tx)
                    .map_err(|e| {
                        crate::KernelError::Serialization(format!(
                            "deserialize capability install: {e}"
                        ))
                    })?;
                let entry = RecordEntry::builder(seq, tx.clone())
                    .context_hash(context_hash)
                    .build();
                Ok(ProcessResult {
                    entry,
                    tool_output: None,
                    had_failures: false,
                    runtime_capability_update,
                    clear_runtime_capabilities: false,
                    tool_decision: None,
                })
            }
            _ => {
                let entry = RecordEntry::builder(seq, tx.clone())
                    .context_hash(context_hash)
                    .build();
                Ok(ProcessResult {
                    entry,
                    tool_output: None,
                    had_failures: false,
                    runtime_capability_update: None,
                    clear_runtime_capabilities: false,
                    tool_decision: None,
                })
            }
        }
    }

    pub(super) fn runtime_capability_update_from_tx(
        tx: &Transaction,
    ) -> Result<Option<RuntimeCapabilityInstall>, serde_json::Error> {
        if tx.tx_type != TransactionType::System {
            return Ok(None);
        }

        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&tx.payload) else {
            return Ok(None);
        };
        let is_capability_install = payload
            .get("system_kind")
            .and_then(serde_json::Value::as_str)
            == Some("capability_install");

        if is_capability_install {
            serde_json::from_value(payload).map(Some)
        } else {
            Ok(None)
        }
    }
}
