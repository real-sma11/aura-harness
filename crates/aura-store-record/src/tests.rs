//! Phase 2 unit tests for the record-log domain.
//!
//! Coverage focus:
//! - [`RecordEntryBuilder`] happy path produces a struct with every
//!   field populated.
//! - [`RecordKind`] serde round-trips through every known variant.
//! - [`RecordKind::Unknown`] is the forward-compat fallback for any
//!   unknown serialised tag.
//! - [`RecordPayload`] serde round-trips `Inline(bytes)` losslessly.
//! - [`RecordLog`] trait is satisfiable by a tiny in-memory mock,
//!   and the mock enforces the strict-monotone-sequence invariant
//!   documented in the module docs.

use std::collections::HashMap;
use std::sync::Mutex;

use aura_core::{AgentId, Decision, ProposalSet, Transaction};
use bytes::Bytes;

use crate::{
    RecordEntry, RecordEntryBuilder, RecordKind, RecordLog, RecordLogError, RecordPayload,
    KERNEL_VERSION,
};

fn build_entry(seq: u64) -> RecordEntry {
    let agent_id = AgentId::generate();
    let tx = Transaction::user_prompt(agent_id, Bytes::from_static(b"hello"));
    RecordEntry::builder(seq, tx)
        .context_hash([7u8; 32])
        .proposals(ProposalSet::new())
        .decision(Decision::new())
        .actions(vec![])
        .effects(vec![])
        .build()
}

#[test]
fn record_entry_builder_populates_all_fields() {
    let entry = build_entry(1);

    assert_eq!(entry.seq, 1);
    assert_eq!(entry.kernel_version, KERNEL_VERSION);
    assert_eq!(entry.context_hash.as_ref(), &[7u8; 32]);
    assert!(entry.actions.is_empty());
    assert!(entry.effects.is_empty());

    // The builder type is publicly re-named here too — make sure the
    // re-export from `record.rs` resolves.
    let _b: RecordEntryBuilder = RecordEntry::builder(2, entry.tx.clone());
}

#[test]
fn record_kind_known_variants_round_trip() {
    let variants = [
        RecordKind::ModelProposal,
        RecordKind::ToolEffect,
        RecordKind::SpawnChild,
        RecordKind::Compaction,
        RecordKind::SteeringDecision,
        RecordKind::PolicyVerdict,
        RecordKind::PermissionsUpdate,
    ];

    for kind in variants {
        let json = serde_json::to_string(&kind).expect("serialise");
        let parsed: RecordKind = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(kind, parsed);
        assert!(!parsed.is_unknown(), "{kind:?} must not be Unknown");
    }
}

#[test]
fn record_kind_synthetic_tag_falls_back_to_unknown() {
    // Forward-compat: an old binary reading a record written by a
    // newer binary whose kind tag is not yet known must deserialise
    // into `RecordKind::Unknown` instead of panicking.
    let json = r#"{"kind":"totally_made_up"}"#;
    let parsed: RecordKind = serde_json::from_str(json).expect("forward-compat fallback");
    assert_eq!(parsed, RecordKind::Unknown);
    assert!(parsed.is_unknown());
}

#[test]
fn record_kind_unknown_with_id_returns_unit_variant() {
    // `#[serde(other)]` requires the fallback variant to be unit, so
    // the numeric id is intentionally discarded by `unknown_with_id`.
    assert_eq!(RecordKind::unknown_with_id(0), RecordKind::Unknown);
    assert_eq!(RecordKind::unknown_with_id(u32::MAX), RecordKind::Unknown);
}

#[test]
fn record_payload_inline_round_trips() {
    let payload = RecordPayload::inline(Bytes::from_static(b"payload-bytes"));
    let json = serde_json::to_string(&payload).expect("serialise");
    let parsed: RecordPayload = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(payload, parsed);
}

#[test]
fn record_payload_summary_round_trips() {
    // Phase 6a: AuditedLite summary variant.
    let payload = RecordPayload::Summary {
        head: Bytes::from_static(b"head-bytes"),
        tail: Bytes::from_static(b"tail-bytes"),
        full_hash: "abcdef".into(),
        full_len: 1024,
    };
    let json = serde_json::to_string(&payload).expect("serialise");
    let parsed: RecordPayload = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(payload, parsed);
}

#[test]
fn summarize_payload_below_threshold_yields_inline() {
    let bytes = b"small";
    let payload = crate::summarize_payload(bytes, 1024);
    match payload {
        RecordPayload::Inline(b) => assert_eq!(b.as_ref(), bytes),
        other => panic!("expected Inline, got {other:?}"),
    }
}

#[test]
fn summarize_payload_above_threshold_yields_summary() {
    // 5 KiB payload, 1 KiB threshold.
    let payload_bytes: Vec<u8> = (0u8..=255).cycle().take(5 * 1024).collect();
    let payload = crate::summarize_payload(&payload_bytes, 1024);
    match payload {
        RecordPayload::Summary {
            head,
            tail,
            full_hash,
            full_len,
        } => {
            assert_eq!(head.len(), 1024);
            assert_eq!(tail.len(), 1024);
            assert_eq!(full_len, payload_bytes.len());
            assert_eq!(head.as_ref(), &payload_bytes[..1024]);
            assert_eq!(tail.as_ref(), &payload_bytes[payload_bytes.len() - 1024..]);
            let expected = blake3::hash(&payload_bytes).to_hex().to_string();
            assert_eq!(full_hash, expected);
        }
        other => panic!("expected Summary, got {other:?}"),
    }
}

#[test]
fn summarize_payload_is_deterministic_across_calls() {
    let payload_bytes: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let first = crate::summarize_payload(&payload_bytes, 1024);
    let second = crate::summarize_payload(&payload_bytes, 1024);
    assert_eq!(first, second, "summarisation must be deterministic");
}

// ---------------------------------------------------------------------
// In-memory `RecordLog` mock — proves the trait is satisfiable and
// that the strict-monotone-sequence invariant documented in
// `log.rs` is enforceable from an implementation.
// ---------------------------------------------------------------------

#[derive(Default)]
struct MemRecordLog {
    inner: Mutex<HashMap<AgentId, Vec<RecordEntry>>>,
}

impl RecordLog for MemRecordLog {
    fn append(&self, agent_id: &AgentId, entry: &RecordEntry) -> Result<(), RecordLogError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| RecordLogError::Backend(format!("mutex poisoned: {e}")))?;
        let log = guard.entry(*agent_id).or_default();
        #[allow(clippy::cast_possible_truncation)]
        let expected = log.len() as u64 + 1;
        if entry.seq != expected {
            return Err(RecordLogError::SeqOutOfOrder {
                agent_id: *agent_id,
                expected,
                actual: entry.seq,
            });
        }
        log.push(entry.clone());
        Ok(())
    }

    fn head_seq(&self, agent_id: &AgentId) -> Result<u64, RecordLogError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| RecordLogError::Backend(format!("mutex poisoned: {e}")))?;
        #[allow(clippy::cast_possible_truncation)]
        let len = guard.get(agent_id).map_or(0, Vec::len) as u64;
        Ok(len)
    }
}

#[test]
fn record_log_mock_appends_and_reports_head() {
    let log = MemRecordLog::default();
    let agent_id = AgentId::generate();

    assert_eq!(log.head_seq(&agent_id).unwrap(), 0);

    log.append(&agent_id, &build_entry(1)).unwrap();
    log.append(&agent_id, &build_entry(2)).unwrap();
    log.append(&agent_id, &build_entry(3)).unwrap();

    assert_eq!(log.head_seq(&agent_id).unwrap(), 3);
}

#[test]
fn record_log_mock_rejects_out_of_order_seq() {
    let log = MemRecordLog::default();
    let agent_id = AgentId::generate();

    log.append(&agent_id, &build_entry(1)).unwrap();

    let err = log
        .append(&agent_id, &build_entry(3))
        .expect_err("non-contiguous seq must be rejected");
    match err {
        RecordLogError::SeqOutOfOrder {
            expected, actual, ..
        } => {
            assert_eq!(expected, 2);
            assert_eq!(actual, 3);
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}
