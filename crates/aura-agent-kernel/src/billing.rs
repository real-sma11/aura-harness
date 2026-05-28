//! Billing / usage attribution helpers.
//!
//! Phase 5 records `parent_agent_id` + `originating_user_id` on every
//! `Delegate` transaction emitted by cross-agent tools (see
//! [`aura_core::types::ToolExecution`]). Billing in aura-os consumes those
//! fields to roll spawned-agent work up to the originating user. This
//! module is the harness-side primitive aura-os calls to walk the chain.

use aura_core::{AgentId, RecordEntry, ToolExecution};
use aura_store::ReadStore;

const LATEST_PARENT_LOOKBACK_LIMIT: usize = 10_000;

/// Walk the parent chain of `agent_id` in child → root order by scanning
/// each agent's record log for the most recent `ToolExecution` carrying a
/// `parent_agent_id`.
///
/// Semantics:
/// - The walk starts with `agent_id` itself (included as the first entry).
/// - The walk terminates at a root (no further `parent_agent_id`), a cycle
///   (parent already seen), or a store error.
/// - `store` errors silently terminate the walk rather than bubbling up —
///   billing rollup should continue with whatever chain was recoverable.
///
/// Typical usage from aura-os:
///
/// ```ignore
/// let chain = aura_kernel::billing::walk_parent_chain(&agent_id, store.as_ref());
/// // chain[0] == agent_id (the leaf)
/// // chain.last() == root (the originating user's first agent)
/// ```
pub fn walk_parent_chain(agent_id: &AgentId, store: &dyn ReadStore) -> Vec<AgentId> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = *agent_id;
    loop {
        if !seen.insert(cursor) {
            break;
        }
        chain.push(cursor);
        match latest_parent(&cursor, store) {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    chain
}

fn latest_parent(agent_id: &AgentId, store: &dyn ReadStore) -> Option<AgentId> {
    // Phase 5 (error-handling polish): the walker is best-effort —
    // billing rollup must never panic the caller — but silent
    // `.ok()?`s used to throw away store-error context, leaving us
    // with no signal that a chain terminated because of a corrupt
    // record. Promote each swallow to a `tracing::warn!` so
    // operators can see chain truncations in the audit trail.
    let head = match store.get_head_seq(*agent_id) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %e,
                agent_id = ?agent_id,
                "billing.walk_parent_chain: get_head_seq failed; truncating chain"
            );
            return None;
        }
    };
    if head == 0 {
        return None;
    }
    let entries = match store.scan_record_descending(*agent_id, head, LATEST_PARENT_LOOKBACK_LIMIT)
    {
        Ok(es) => es,
        Err(e) => {
            tracing::warn!(
                error = %e,
                agent_id = ?agent_id,
                head,
                limit = LATEST_PARENT_LOOKBACK_LIMIT,
                "billing.walk_parent_chain: descending record scan failed; truncating chain"
            );
            return None;
        }
    };
    for entry in &entries {
        if let Some(parent) = parent_from_entry(entry) {
            return Some(parent);
        }
    }
    if head > u64::try_from(LATEST_PARENT_LOOKBACK_LIMIT).unwrap_or(u64::MAX) {
        // TODO(parent_index): persist child -> latest parent metadata so very
        // old parent markers are found without scanning unbounded history.
        tracing::warn!(
            agent_id = ?agent_id,
            head,
            limit = LATEST_PARENT_LOOKBACK_LIMIT,
            "billing.walk_parent_chain: no parent marker in bounded tail scan; truncating chain"
        );
    }
    None
}

fn parent_from_entry(entry: &RecordEntry) -> Option<AgentId> {
    for effect in &entry.effects {
        if let Ok(exec) = serde_json::from_slice::<ToolExecution>(&effect.payload) {
            return Some(exec.parent_agent_id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{
        AgentStatus, Effect, EffectKind, EffectStatus, RuntimeCapabilityInstall, ToolGateVerdict,
        Transaction, TransactionType, UserToolDefaults,
    };
    use aura_store::{DequeueToken, StoreError};
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory stub that only implements [`ReadStore`] — the
    /// sealed `WriteStore` surface is off-limits outside `aura-store`
    /// (Wave 2 T3, Invariant §10). Tests seed records via the inherent
    /// [`MemStore::insert`] helper rather than through the trait.
    #[derive(Default)]
    struct MemStore {
        heads: Mutex<HashMap<AgentId, u64>>,
        records: Mutex<HashMap<(AgentId, u64), RecordEntry>>,
        scan_calls: Mutex<Vec<(AgentId, u64, usize)>>,
        descending_scan_calls: Mutex<Vec<(AgentId, u64, usize)>>,
    }

    impl MemStore {
        fn insert(&self, agent_id: AgentId, seq: u64, entry: RecordEntry) {
            self.heads.lock().unwrap().insert(agent_id, seq);
            self.records.lock().unwrap().insert((agent_id, seq), entry);
        }
    }

    impl aura_store::ReadStore for MemStore {
        fn enqueue_tx(&self, _tx: &Transaction) -> Result<(), StoreError> {
            Ok(())
        }
        fn dequeue_tx(
            &self,
            _agent_id: AgentId,
        ) -> Result<Option<(DequeueToken, Transaction)>, StoreError> {
            Ok(None)
        }
        fn get_head_seq(&self, agent_id: AgentId) -> Result<u64, StoreError> {
            Ok(self
                .heads
                .lock()
                .unwrap()
                .get(&agent_id)
                .copied()
                .unwrap_or(0))
        }
        fn scan_record(
            &self,
            agent_id: AgentId,
            from_seq: u64,
            limit: usize,
        ) -> Result<Vec<RecordEntry>, StoreError> {
            self.scan_calls
                .lock()
                .unwrap()
                .push((agent_id, from_seq, limit));
            let head = self.get_head_seq(agent_id)?;
            let mut out = Vec::new();
            for seq in from_seq..=head {
                if out.len() >= limit {
                    break;
                }
                if let Some(entry) = self.records.lock().unwrap().get(&(agent_id, seq)) {
                    out.push(entry.clone());
                }
            }
            Ok(out)
        }
        fn scan_record_descending(
            &self,
            agent_id: AgentId,
            from_seq: u64,
            limit: usize,
        ) -> Result<Vec<RecordEntry>, StoreError> {
            self.descending_scan_calls
                .lock()
                .unwrap()
                .push((agent_id, from_seq, limit));
            if from_seq == 0 || limit == 0 {
                return Ok(Vec::new());
            }

            let window = u64::try_from(limit).unwrap_or(u64::MAX);
            let min_seq = from_seq.saturating_sub(window.saturating_sub(1)).max(1);
            let records = self.records.lock().unwrap();
            let mut out = Vec::new();
            for seq in (min_seq..=from_seq).rev() {
                if let Some(entry) = records.get(&(agent_id, seq)) {
                    out.push(entry.clone());
                }
            }
            Ok(out)
        }
        fn get_record_entry(&self, agent_id: AgentId, seq: u64) -> Result<RecordEntry, StoreError> {
            self.records
                .lock()
                .unwrap()
                .get(&(agent_id, seq))
                .cloned()
                .ok_or(StoreError::RecordEntryNotFound(agent_id, seq))
        }
        fn get_agent_status(&self, _agent_id: AgentId) -> Result<AgentStatus, StoreError> {
            Ok(AgentStatus::Active)
        }
        fn get_runtime_capabilities(
            &self,
            _agent_id: AgentId,
        ) -> Result<Option<RuntimeCapabilityInstall>, StoreError> {
            Ok(None)
        }
        fn set_agent_status(
            &self,
            _agent_id: AgentId,
            _status: AgentStatus,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn has_pending_tx(&self, _agent_id: AgentId) -> Result<bool, StoreError> {
            Ok(false)
        }
        fn get_inbox_depth(&self, _agent_id: AgentId) -> Result<u64, StoreError> {
            Ok(0)
        }
        fn get_user_tool_defaults(
            &self,
            _user_id: &str,
        ) -> Result<Option<UserToolDefaults>, StoreError> {
            Ok(None)
        }
        fn put_user_tool_defaults(
            &self,
            _user_id: &str,
            _defaults: &UserToolDefaults,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        fn delete_user_tool_defaults(&self, _user_id: &str) -> Result<(), StoreError> {
            Ok(())
        }
        fn try_claim_agent_processing(&self, _agent_id: AgentId) -> Result<bool, StoreError> {
            Ok(true)
        }
        fn release_agent_processing(&self, _agent_id: AgentId) -> Result<(), StoreError> {
            Ok(())
        }
        fn is_agent_processing(&self, _agent_id: AgentId) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    fn parent_entry(seq: u64, agent: AgentId, parent: AgentId) -> RecordEntry {
        let tx = Transaction::new_chained(
            agent,
            TransactionType::System,
            Bytes::from(b"parent-marker".to_vec()),
            None,
        );
        let exec = ToolExecution {
            tool_use_id: "spawn".into(),
            tool: "spawn_agent".into(),
            args: serde_json::json!({}),
            decision: ToolGateVerdict::Approved,
            reason: None,
            result: None,
            is_error: false,
            parent_agent_id: parent,
            originating_user_id: "user-root".into(),
        };
        let effect_payload = serde_json::to_vec(&exec).unwrap();
        let effect = Effect::new(
            aura_core::ActionId::generate(),
            EffectKind::Agreement,
            EffectStatus::Committed,
            Bytes::from(effect_payload),
        );
        RecordEntry::builder(seq, tx)
            .context_hash([0u8; 32])
            .effects(vec![effect])
            .build()
    }

    #[test]
    fn three_deep_chain_walks_to_root() {
        let store = MemStore::default();
        let root = AgentId::generate();
        let mid = AgentId::generate();
        let leaf = AgentId::generate();

        store.insert(mid, 1, parent_entry(1, mid, root));
        store.insert(leaf, 1, parent_entry(1, leaf, mid));

        let chain = walk_parent_chain(&leaf, &store);
        assert_eq!(chain, vec![leaf, mid, root]);
    }

    #[test]
    fn root_only_returns_self() {
        let store = MemStore::default();
        let only = AgentId::generate();
        let chain = walk_parent_chain(&only, &store);
        assert_eq!(chain, vec![only]);
    }

    #[test]
    fn cycle_terminates() {
        // Forge a record where A claims B as parent and B claims A — the
        // walker must halt rather than looping forever.
        let store = MemStore::default();
        let a = AgentId::generate();
        let b = AgentId::generate();
        store.insert(a, 1, parent_entry(1, a, b));
        store.insert(b, 1, parent_entry(1, b, a));
        let chain = walk_parent_chain(&a, &store);
        assert_eq!(chain, vec![a, b]);
    }

    #[test]
    fn latest_parent_uses_bounded_tail_lookup_for_large_heads() {
        let store = MemStore::default();
        let child = AgentId::generate();
        let parent = AgentId::generate();
        let large_head = 1_000_000;

        store.insert(child, large_head, parent_entry(large_head, child, parent));

        let chain = walk_parent_chain(&child, &store);
        assert_eq!(chain, vec![child, parent]);
        assert!(
            store.scan_calls.lock().unwrap().is_empty(),
            "billing should use descending tail lookup instead of full forward scans"
        );
        assert_eq!(
            store.descending_scan_calls.lock().unwrap().as_slice(),
            &[(child, large_head, LATEST_PARENT_LOOKBACK_LIMIT)]
        );
    }
}
