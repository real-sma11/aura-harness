use super::*;
use proptest::prelude::*;

fn arb_agent_id() -> impl Strategy<Value = AgentId> {
    any::<[u8; 32]>().prop_map(AgentId::new)
}

fn arb_meta_field() -> impl Strategy<Value = MetaField> {
    prop_oneof![
        Just(MetaField::HeadSeq),
        Just(MetaField::InboxHead),
        Just(MetaField::InboxTail),
        Just(MetaField::Status),
        Just(MetaField::SchemaVersion),
    ]
}

proptest! {
    #[test]
    fn proptest_record_key_roundtrip(
        agent_id in arb_agent_id(),
        seq in any::<u64>(),
    ) {
        let key = RecordKey::new(agent_id, seq);
        let encoded = key.encode();
        let decoded = RecordKey::decode(&encoded).unwrap();
        prop_assert_eq!(key, decoded);
    }

    #[test]
    fn proptest_agent_meta_key_roundtrip(
        agent_id in arb_agent_id(),
        field in arb_meta_field(),
    ) {
        let key = AgentMetaKey::new(agent_id, field);
        let encoded = key.encode();
        let decoded = AgentMetaKey::decode(&encoded).unwrap();
        prop_assert_eq!(key, decoded);
    }

    #[test]
    fn proptest_inbox_key_roundtrip(
        agent_id in arb_agent_id(),
        inbox_seq in any::<u64>(),
    ) {
        let key = InboxKey::new(agent_id, inbox_seq);
        let encoded = key.encode();
        let decoded = InboxKey::decode(&encoded).unwrap();
        prop_assert_eq!(key, decoded);
    }

    #[test]
    fn proptest_record_key_ordering_preserved(
        agent_id in arb_agent_id(),
        seq_a in any::<u64>(),
        seq_b in any::<u64>(),
    ) {
        let key_a = RecordKey::new(agent_id, seq_a).encode();
        let key_b = RecordKey::new(agent_id, seq_b).encode();
        prop_assert_eq!(key_a.cmp(&key_b), seq_a.cmp(&seq_b));
    }

    #[test]
    fn proptest_inbox_key_ordering_preserved(
        agent_id in arb_agent_id(),
        seq_a in any::<u64>(),
        seq_b in any::<u64>(),
    ) {
        let key_a = InboxKey::new(agent_id, seq_a).encode();
        let key_b = InboxKey::new(agent_id, seq_b).encode();
        prop_assert_eq!(key_a.cmp(&key_b), seq_a.cmp(&seq_b));
    }
}

#[test]
fn record_key_roundtrip() {
    let agent = AgentId::generate();
    let key = RecordKey::new(agent, 42);
    let encoded = key.encode();
    let decoded = RecordKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
}

#[test]
fn record_key_ordering() {
    let agent = AgentId::new([1u8; 32]);
    let key1 = RecordKey::new(agent, 1).encode();
    let key2 = RecordKey::new(agent, 2).encode();
    let key10 = RecordKey::new(agent, 10).encode();

    assert!(key1 < key2);
    assert!(key2 < key10);
}

#[test]
fn agent_meta_key_roundtrip() {
    let agent = AgentId::generate();
    let key = AgentMetaKey::head_seq(agent);
    let encoded = key.encode();
    let decoded = AgentMetaKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
}

#[test]
fn inbox_key_roundtrip() {
    let agent = AgentId::generate();
    let key = InboxKey::new(agent, 100);
    let encoded = key.encode();
    let decoded = InboxKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
}

#[test]
fn inbox_key_ordering() {
    let agent = AgentId::new([2u8; 32]);
    let key1 = InboxKey::new(agent, 1).encode();
    let key2 = InboxKey::new(agent, 2).encode();

    assert!(key1 < key2);
}

#[test]
fn record_key_seq_zero() {
    let agent = AgentId::new([0u8; 32]);
    let key = RecordKey::new(agent, 0);
    let encoded = key.encode();
    let decoded = RecordKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
}

#[test]
fn record_key_seq_max() {
    let agent = AgentId::new([0xFF; 32]);
    let key = RecordKey::new(agent, u64::MAX);
    let encoded = key.encode();
    let decoded = RecordKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
    assert_eq!(decoded.seq, u64::MAX);
}

#[test]
fn inbox_key_seq_zero_all_zero_agent() {
    let agent = AgentId::new([0u8; 32]);
    let key = InboxKey::new(agent, 0);
    let encoded = key.encode();
    let decoded = InboxKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
}

#[test]
fn inbox_key_seq_max_all_ff_agent() {
    let agent = AgentId::new([0xFF; 32]);
    let key = InboxKey::new(agent, u64::MAX);
    let encoded = key.encode();
    let decoded = InboxKey::decode(&encoded).unwrap();
    assert_eq!(key, decoded);
    assert_eq!(decoded.inbox_seq, u64::MAX);
}

#[test]
fn agent_meta_key_all_zero_agent() {
    let agent = AgentId::new([0u8; 32]);
    for field in [
        MetaField::HeadSeq,
        MetaField::InboxHead,
        MetaField::InboxTail,
        MetaField::Status,
        MetaField::SchemaVersion,
    ] {
        let key = AgentMetaKey::new(agent, field);
        let encoded = key.encode();
        let decoded = AgentMetaKey::decode(&encoded).unwrap();
        assert_eq!(key, decoded);
    }
}

#[test]
fn agent_meta_key_all_ff_agent() {
    let agent = AgentId::new([0xFF; 32]);
    for field in [
        MetaField::HeadSeq,
        MetaField::InboxHead,
        MetaField::InboxTail,
        MetaField::Status,
        MetaField::SchemaVersion,
    ] {
        let key = AgentMetaKey::new(agent, field);
        let encoded = key.encode();
        let decoded = AgentMetaKey::decode(&encoded).unwrap();
        assert_eq!(key, decoded);
    }
}

#[test]
fn record_key_decode_wrong_length() {
    assert!(RecordKey::decode(&[]).is_err());
    assert!(RecordKey::decode(&[prefix::RECORD]).is_err());
    assert!(RecordKey::decode(&[prefix::RECORD; 100]).is_err());
}

#[test]
fn record_key_decode_wrong_prefix() {
    let agent = AgentId::new([1u8; 32]);
    let mut encoded = RecordKey::new(agent, 1).encode();
    encoded[0] = b'X';
    assert!(RecordKey::decode(&encoded).is_err());
}

#[test]
fn inbox_key_decode_wrong_length() {
    assert!(InboxKey::decode(&[]).is_err());
    assert!(InboxKey::decode(&[prefix::INBOX; 2]).is_err());
}

#[test]
fn inbox_key_decode_wrong_prefix() {
    let agent = AgentId::new([1u8; 32]);
    let mut encoded = InboxKey::new(agent, 1).encode();
    encoded[0] = b'X';
    assert!(InboxKey::decode(&encoded).is_err());
}

#[test]
fn agent_meta_key_decode_wrong_length() {
    assert!(AgentMetaKey::decode(&[]).is_err());
    assert!(AgentMetaKey::decode(&[prefix::AGENT_META; 2]).is_err());
}

#[test]
fn agent_meta_key_decode_invalid_field() {
    let agent = AgentId::new([1u8; 32]);
    let mut encoded = AgentMetaKey::head_seq(agent).encode();
    encoded[33] = 0xFF; // Invalid field discriminant
    assert!(AgentMetaKey::decode(&encoded).is_err());
}

#[test]
fn meta_field_byte_roundtrip() {
    #[allow(deprecated)]
    let fields = [
        MetaField::HeadSeq,
        MetaField::InboxHead,
        MetaField::InboxTail,
        MetaField::Status,
        MetaField::SchemaVersion,
        // Added in d762e84 (store-backed agent processing claims) —
        // the byte 5 slot is now occupied, so `meta_field_from_invalid_byte`
        // below probes byte 6 instead.
        MetaField::ProcessingClaim,
    ];
    for field in fields {
        let byte = field.as_byte();
        let parsed = MetaField::from_byte(byte).unwrap();
        assert_eq!(field, parsed);
    }
}

#[test]
fn meta_field_from_invalid_byte() {
    // Byte 5 is `MetaField::ProcessingClaim` (added in d762e84).
    // The first invalid byte is now 6.
    assert!(MetaField::from_byte(6).is_none());
    assert!(MetaField::from_byte(255).is_none());
}
