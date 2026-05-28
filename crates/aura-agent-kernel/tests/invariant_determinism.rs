//! Invariant §6 — deterministic context.
//!
//! `hash_tx_with_window(tx, window)` must be a pure function of its
//! inputs: re-running it against identical inputs produces the same
//! hash, and any observable change to the inputs (different tx, a
//! reordered window, an injected no-op entry) produces a different
//! hash. This suite exercises those properties directly on the
//! canonical function exposed from `aura-kernel`.
//!
//! Enforcement target: Invariant §6 in `docs/invariants.md`.

use aura_core::{
    AgentId, ContextHash, Decision, ProposalSet, RecordEntry, Transaction, TransactionType,
};
use aura_agent_kernel::hash_tx_with_window;
use proptest::prelude::*;

fn build_entry(
    seq: u64,
    agent_id: AgentId,
    tx_type: TransactionType,
    payload: &[u8],
) -> RecordEntry {
    // Chain `context_hash` off `(seq, payload)` so reordering a window
    // is observable by `hash_tx_with_window` (which only mixes in each
    // entry's `context_hash`, not its seq or payload directly).
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seq.to_le_bytes());
    hasher.update(payload);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(hasher.finalize().as_bytes());

    let tx = Transaction::new_chained(agent_id, tx_type, payload.to_vec(), None);
    RecordEntry::builder(seq, tx)
        .context_hash(ContextHash::from(bytes))
        .proposals(ProposalSet::new())
        .decision(Decision::new())
        .build()
}

fn tx_type_from_idx(idx: u8) -> TransactionType {
    match idx % 9 {
        0 => TransactionType::UserPrompt,
        1 => TransactionType::AgentMsg,
        2 => TransactionType::Trigger,
        3 => TransactionType::ActionResult,
        4 => TransactionType::System,
        5 => TransactionType::SessionStart,
        6 => TransactionType::ToolProposal,
        7 => TransactionType::ToolExecution,
        _ => TransactionType::ProcessComplete,
    }
}

const FIXED_AGENT_BYTES: [u8; 32] = [0x42u8; 32];

fn fixed_agent() -> AgentId {
    AgentId::new(FIXED_AGENT_BYTES)
}

fn arb_transaction() -> impl Strategy<Value = Transaction> {
    // Keep the strategy small and deterministic: random `tx_type`
    // selector and a bounded payload of arbitrary bytes. `AgentId`
    // is fixed inside each test case so the same input values
    // always produce the same transaction.
    (any::<u8>(), prop::collection::vec(any::<u8>(), 0..64)).prop_map(|(kind_idx, payload)| {
        Transaction::new_chained(fixed_agent(), tx_type_from_idx(kind_idx), payload, None)
    })
}

fn arb_window() -> impl Strategy<Value = Vec<RecordEntry>> {
    let agent_id = fixed_agent();
    prop::collection::vec(
        (any::<u8>(), prop::collection::vec(any::<u8>(), 0..32)),
        0..6,
    )
    .prop_map(move |entries| {
        entries
            .into_iter()
            .enumerate()
            .map(|(i, (kind_idx, payload))| {
                build_entry(
                    (i as u64) + 1,
                    agent_id,
                    tx_type_from_idx(kind_idx),
                    &payload,
                )
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        .. ProptestConfig::default()
    })]

    /// Determinism: repeated calls with identical inputs produce
    /// identical hashes. Covers Invariant §6 directly.
    #[test]
    fn hash_tx_with_window_is_deterministic(tx in arb_transaction(), window in arb_window()) {
        let first = hash_tx_with_window(&tx, &window).expect("serialize tx");
        for _ in 0..4 {
            let again = hash_tx_with_window(&tx, &window).expect("serialize tx");
            prop_assert_eq!(first, again);
        }
    }

    /// Swapping any two adjacent entries in a non-trivial window
    /// changes the hash. The `window` strategy is filtered to
    /// windows of length >= 2 where the two entries differ in
    /// `context_hash` (same payload + same seq would otherwise hash
    /// identically, which is indistinguishable from a no-op swap).
    #[test]
    fn swapping_adjacent_window_entries_changes_hash(
        tx in arb_transaction(),
        window in arb_window().prop_filter(
            "window must have two adjacent entries with distinct context_hashes",
            |w| w.windows(2).any(|pair| pair[0].context_hash != pair[1].context_hash),
        ),
    ) {
        let original = hash_tx_with_window(&tx, &window).expect("serialize tx");

        let mut swapped = window.clone();
        // Find the first adjacent pair with distinct context hashes and swap.
        let pivot = swapped
            .windows(2)
            .position(|pair| pair[0].context_hash != pair[1].context_hash)
            .expect("filter guarantees at least one such pair");
        swapped.swap(pivot, pivot + 1);

        let after = hash_tx_with_window(&tx, &swapped).expect("serialize tx");
        prop_assert_ne!(original, after);
    }

    /// Inserting an additional no-op entry at the end of the
    /// window changes the hash (the extra `context_hash` byte-chunk
    /// gets folded into the blake3 accumulator).
    #[test]
    fn inserting_noop_entry_changes_hash(tx in arb_transaction(), window in arb_window()) {
        let original = hash_tx_with_window(&tx, &window).expect("serialize tx");

        let mut extended = window.clone();
        extended.push(build_entry(
            (extended.len() as u64) + 1,
            fixed_agent(),
            TransactionType::System,
            b"noop",
        ));

        let after = hash_tx_with_window(&tx, &extended).expect("serialize tx");
        prop_assert_ne!(original, after);
    }

    /// Any byte-level change to the transaction produces a different
    /// hash. We mutate by switching the `tx_type` (`serde_json` names
    /// differ) or by extending the payload, which are both
    /// serialization-visible changes.
    #[test]
    fn hash_differs_for_different_tx(
        tx in arb_transaction(),
        window in arb_window(),
        flip in any::<bool>(),
    ) {
        let h_original = hash_tx_with_window(&tx, &window).expect("serialize tx");

        let mutated = if flip {
            // Bump tx_type to the next variant — always a serialization-
            // visible change because the enum serializes as a distinct
            // string.
            let next_kind_idx = (tx_type_from_idx_as_u8(&tx) + 1) % 9;
            Transaction::new_chained(
                tx.agent_id,
                tx_type_from_idx(next_kind_idx),
                tx.payload.to_vec(),
                None,
            )
        } else {
            // Append a single byte so the payload is strictly different.
            let mut extended = tx.payload.to_vec();
            extended.push(0xAB);
            Transaction::new_chained(tx.agent_id, tx.tx_type, extended, None)
        };

        let h_mutated = hash_tx_with_window(&mutated, &window).expect("serialize tx");
        prop_assert_ne!(h_original, h_mutated);
    }
}

fn tx_type_from_idx_as_u8(tx: &Transaction) -> u8 {
    match tx.tx_type {
        TransactionType::UserPrompt => 0,
        TransactionType::AgentMsg => 1,
        TransactionType::Trigger => 2,
        TransactionType::ActionResult => 3,
        TransactionType::System => 4,
        TransactionType::SessionStart => 5,
        TransactionType::ToolProposal => 6,
        TransactionType::ToolExecution => 7,
        TransactionType::ProcessComplete => 8,
        // Any newer variant — treat as 8 so `+1 % 9` stays a valid mutation.
        _ => 8,
    }
}
