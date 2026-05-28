//! Streaming handle for reasoning calls.
//!
//! [`ReasonStreamHandle`] is returned alongside a streaming response so the
//! caller can finalize the record entry once the stream completes. Both
//! finalization methods consume `self`, making double-recording impossible
//! by construction (Invariant §3).

use crate::context::hash_tx_with_window;
use aura_core::{AgentId, RecordEntry, Transaction, TransactionType};
use aura_reasoner::{StopReason, Usage};
use aura_store::Store;
use std::sync::{Arc, Mutex};

/// Handle returned alongside a streaming response so the caller can finalize
/// the record entry once the stream completes.
///
/// The handle reserves a sequence number and snapshots the record window at
/// construction time so that `record_completed`/`record_failed` can produce
/// a context hash deterministically from the state observed at the start of
/// the streaming call (Invariant §6).
///
/// Both finalization methods consume `self` so that Invariant §3 holds by
/// construction: a given handle can be finalized at most once, and the
/// `Drop` path on the caller side (e.g. `RecordingStream`) is responsible
/// for ensuring that a record is always appended exactly once.
pub struct ReasonStreamHandle {
    pub(super) kernel_store: Arc<dyn Store>,
    pub(super) agent_id: AgentId,
    pub(super) seq_counter: Arc<Mutex<u64>>,
    pub(super) window: Vec<RecordEntry>,
}

impl ReasonStreamHandle {
    fn next_seq(&self) -> Result<u64, crate::KernelError> {
        let head_seq = self
            .kernel_store
            .get_head_seq(self.agent_id)
            .map_err(|e| crate::KernelError::Store(format!("get_head_seq: {e}")))?;
        let next = head_seq + 1;
        let mut seq = self
            .seq_counter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq = next + 1;
        Ok(next)
    }

    fn append(self, tx: Transaction) -> Result<RecordEntry, crate::KernelError> {
        // Allocate the sequence atomically with the append so streaming
        // reasoning interleaves cleanly with other kernel paths that
        // also draw from `Kernel::seq`. The context hash comes from the
        // *snapshot* window captured when `reason_streaming` started,
        // which is the state the model actually reasoned over
        // (Invariant §6).
        let seq = self.next_seq()?;
        let context_hash = hash_tx_with_window(&tx, &self.window)?;
        let entry = RecordEntry::builder(seq, tx)
            .context_hash(context_hash)
            .build();

        self.kernel_store
            .append_entry_direct(self.agent_id, seq, &entry)
            .map_err(|e| crate::KernelError::Store(format!("append_entry_direct: {e}")))?;

        Ok(entry)
    }

    /// Record a successfully completed streaming response.
    ///
    /// Consumes the handle so that double-finalization is impossible by
    /// construction.
    ///
    /// # Errors
    /// Returns error if serialization or store append fails.
    pub fn record_completed(
        self,
        model: &str,
        stop_reason: StopReason,
        usage: &Usage,
        tool_uses: &[String],
    ) -> Result<RecordEntry, crate::KernelError> {
        let reasoning_payload = serde_json::json!({
            "model": model,
            "stop_reason": format!("{stop_reason:?}"),
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "tool_uses": tool_uses,
        });
        let payload_bytes = serde_json::to_vec(&reasoning_payload)
            .map_err(|e| crate::KernelError::Serialization(e.to_string()))?;

        let tx = Transaction::new_chained(
            self.agent_id,
            TransactionType::Reasoning,
            payload_bytes,
            None,
        );

        self.append(tx)
    }

    /// Record a failed streaming response.
    ///
    /// Consumes the handle so that double-finalization is impossible by
    /// construction.
    ///
    /// # Errors
    /// Returns error if serialization or store append fails.
    pub fn record_failed(self, error: &str) -> Result<RecordEntry, crate::KernelError> {
        let reasoning_payload = serde_json::json!({
            "error": error,
            "status": "failed",
        });
        let payload_bytes = serde_json::to_vec(&reasoning_payload)
            .map_err(|e| crate::KernelError::Serialization(e.to_string()))?;

        let tx = Transaction::new_chained(
            self.agent_id,
            TransactionType::Reasoning,
            payload_bytes,
            None,
        );

        self.append(tx)
    }
}
