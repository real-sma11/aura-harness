//! Context building for the kernel.

use aura_core::{hash, ContextHash, RecordEntry, Transaction};
use aura_reasoner::RecordSummary;
use tracing::debug;

/// Canonical context-hash function for every kernel processing path.
///
/// Implements Invariant §6 literally:
///
/// ```text
/// context_hash = hash(serialize(tx)
///                  || seq[0].context_hash
///                  || seq[1].context_hash
///                  || ...)
/// ```
///
/// Note that only the per-entry `context_hash` participates — neither the
/// entry's `seq`, tx type, nor payload is mixed in. The chain of prior
/// `context_hash` values already encodes that history transitively, which
/// keeps the hash stable under inconsequential representation changes
/// while still diverging on any semantic change to the record.
///
/// # Errors
/// Returns an error if the transaction cannot be serialized.
///
/// Exposed as `pub` so the invariant test suite in
/// `crates/aura-kernel/tests/invariant_determinism.rs` (Phase 10 / Wave 7)
/// can assert Invariant §6 directly against the canonical function without
/// going through `ContextBuilder`. The function is pure — it has no side
/// effects and no hidden state — so widening its visibility does not expand
/// the kernel's production surface.
pub fn hash_tx_with_window(
    tx: &Transaction,
    window: &[RecordEntry],
) -> Result<ContextHash, crate::KernelError> {
    let tx_bytes = serde_json::to_vec(tx)
        .map_err(|e| crate::KernelError::Serialization(format!("serialize tx: {e}")))?;
    let mut hasher = hash::Hasher::new();
    hasher.update(&tx_bytes);
    for entry in window {
        hasher.update(entry.context_hash.as_ref());
    }
    Ok(ContextHash::from(hasher.finalize()))
}

/// Context for kernel processing.
#[derive(Debug, Clone)]
pub struct Context {
    /// Hash of the context inputs
    pub context_hash: ContextHash,
    /// Record window summaries for the reasoner
    pub record_summaries: Vec<RecordSummary>,
}

/// Builder for kernel context.
pub struct ContextBuilder {
    tx: Transaction,
    record_window: Vec<RecordEntry>,
}

impl ContextBuilder {
    /// Create a new context builder.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction cannot be serialized.
    pub fn new(tx: &Transaction) -> Result<Self, serde_json::Error> {
        // Pre-flight the serialization so the eventual `build()` cannot fail.
        let _ = serde_json::to_vec(tx)?;
        Ok(Self {
            tx: tx.clone(),
            record_window: Vec::new(),
        })
    }

    /// Add record window entries.
    #[must_use]
    pub fn with_record_window(mut self, entries: Vec<RecordEntry>) -> Self {
        self.record_window = entries;
        self
    }

    /// Build the context.
    ///
    /// # Errors
    ///
    /// Returns [`crate::KernelError::Internal`] when the canonical
    /// `hash_tx_with_window` function fails (for example, if transaction
    /// serialization fails). The previous implementation silently fell back
    /// to an all-zero context hash, which would have violated Invariant §6
    /// by producing two distinct transactions with identical context hashes.
    pub fn build(self) -> Result<Context, crate::KernelError> {
        // Delegate to the canonical `hash_tx_with_window` so every kernel
        // path agrees on the formula from Invariant §6.
        let context_hash = hash_tx_with_window(&self.tx, &self.record_window)
            .map_err(|e| crate::KernelError::Internal(format!("context hash: {e}")))?;

        // Build record summaries for reasoner
        let record_summaries: Vec<RecordSummary> = self
            .record_window
            .iter()
            .map(|entry| {
                let action_kinds: Vec<_> = entry.actions.iter().map(|a| a.kind).collect();

                // Opaque fingerprint of the payload: first 16 hex chars of the
                // BLAKE3 digest. We keep the field name for log compatibility
                // but no longer leak plaintext bytes (which could include
                // secrets, PII, or raw prompts) into record summaries that
                // fan out through the reasoner and tracing. (Wave 5 / T6.)
                let digest = blake3::hash(&entry.tx.payload);
                let payload_summary = Some(format!("blake3:{}", &digest.to_hex()[..16]));

                RecordSummary {
                    seq: entry.seq,
                    tx_kind: format!("{:?}", entry.tx.tx_type),
                    action_kinds,
                    payload_summary,
                }
            })
            .collect();

        debug!(
            hash = hex::encode(&context_hash.as_ref()[..8]),
            window_size = record_summaries.len(),
            "Context built"
        );

        Ok(Context {
            context_hash,
            record_summaries,
        })
    }
}

#[cfg(test)]
mod tests;
