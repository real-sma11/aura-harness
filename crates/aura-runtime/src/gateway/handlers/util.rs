//! Shared identifier parsing helpers for router handlers.
//!
//! Phase 1 (refactor) extracts the duplicated `parse_agent_id` helpers
//! from `memory.rs`, `skills.rs`, `tool_permissions.rs`, and `tx.rs`
//! into one canonical implementation. The function accepts both UUID
//! strings (matching the memory + skills surface) and the 32-byte hex
//! form (matching the tx + tool-permissions surface) so every router
//! endpoint speaks the same agent-id grammar.

use super::super::errors::ApiError;
use aura_core_types::AgentId;

/// Parse an agent id from a path or body field.
///
/// Accepts either:
/// - a UUID string (8-4-4-4-12 hyphenated form),
/// - a 32-byte lowercase hex string (the canonical [`AgentId::to_hex`]
///   round-trip), or
/// - a partition key of the form `"{template}::{instance}"` where the
///   template prefix is itself a UUID or 32-byte hex string. The
///   `::suffix` is stripped before parsing so callers that haven't
///   noticed they're routing through aura-os' `harness_agent_id`
///   partition still resolve to the underlying template id (the
///   harness keys long-term memory by template, not partition — see
///   [`crate::gateway::session::Session::memory_agent_id`]).
///
/// The partition key also supports a **three-segment** form
/// `"{template}::{instance}::{session}"` which aura-os emits on chat
/// routes when a single agent instance is partitioned per logical
/// storage `session_id` so concurrent turns on the same instance can
/// run in parallel (see the `parallel-session-chats` cross-repo plan).
/// The implementation uses [`str::split_once`] with `"::"` as the
/// separator, so it only splits on the *first* `::` and the entire
/// `instance::session` tail is dropped in one go — the head returned
/// to the UUID/hex parser is the bare template id for all three
/// forms (bare, two-segment, and three-segment) without any
/// additional logic. The harness reserves the right to add more
/// trailing segments in the future without breaking this parser.
///
/// Errors return [`ApiError::bad_request`] with `400 Bad Request` and a
/// JSON body `{ "error": "invalid agent_id: <reason>" }`.
pub(crate) fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    let head = s.split_once("::").map_or(s, |(h, _)| h);
    if let Ok(uuid) = uuid::Uuid::parse_str(head) {
        return Ok(AgentId::from_uuid(uuid));
    }
    AgentId::from_hex(head).map_err(|e| ApiError::bad_request(format!("invalid agent_id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_STR: &str = "12345678-1234-5678-1234-567812345678";

    #[test]
    fn accepts_bare_uuid() {
        let parsed = parse_agent_id(UUID_STR).expect("uuid should parse");
        let expected =
            AgentId::from_uuid(uuid::Uuid::parse_str(UUID_STR).expect("static uuid is valid"));
        assert_eq!(parsed, expected);
    }

    #[test]
    fn accepts_bare_hex() {
        let id = AgentId::new([7; 32]);
        let parsed = parse_agent_id(&id.to_hex()).expect("hex should parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn accepts_uuid_partition_default_suffix() {
        let parsed =
            parse_agent_id(&format!("{UUID_STR}::default")).expect("partitioned uuid should parse");
        let expected =
            AgentId::from_uuid(uuid::Uuid::parse_str(UUID_STR).expect("static uuid is valid"));
        assert_eq!(parsed, expected);
    }

    #[test]
    fn accepts_uuid_partition_instance_suffix() {
        let instance = "abcdef01-2345-6789-abcd-ef0123456789";
        let parsed = parse_agent_id(&format!("{UUID_STR}::{instance}"))
            .expect("partitioned uuid should parse");
        let expected =
            AgentId::from_uuid(uuid::Uuid::parse_str(UUID_STR).expect("static uuid is valid"));
        assert_eq!(parsed, expected);
    }

    #[test]
    fn accepts_hex_partition_default_suffix() {
        let id = AgentId::new([3; 32]);
        let parsed = parse_agent_id(&format!("{}::default", id.to_hex()))
            .expect("partitioned hex should parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn rejects_garbage() {
        let err = parse_agent_id("not-an-id").expect_err("garbage should fail");
        assert!(format!("{err:?}").contains("invalid agent_id"));
    }

    #[test]
    fn rejects_garbage_with_partition_suffix() {
        let err = parse_agent_id("not-an-id::default").expect_err("garbage prefix should fail");
        assert!(format!("{err:?}").contains("invalid agent_id"));
    }

    /// Three-segment partition strings of the form
    /// `"{template}::{instance}::{session}"` resolve to the underlying
    /// template `AgentId`. aura-os started emitting this form on chat
    /// routes to give every storage `session_id` its own harness WS /
    /// record-log key while keeping a canonical template identity for
    /// deriving the appropriate project-agent memory partition. The
    /// parser uses `split_once("::")`, so the entire
    /// `instance::session` tail is dropped in a single split — no
    /// special-casing for the third segment is required.
    #[test]
    fn accepts_three_segment_partition() {
        let instance = "abcdef01-2345-6789-abcd-ef0123456789";
        let session = "11111111-2222-3333-4444-555555555555";
        let parsed = parse_agent_id(&format!("{UUID_STR}::{instance}::{session}"))
            .expect("three-segment partitioned uuid should parse");
        let expected =
            AgentId::from_uuid(uuid::Uuid::parse_str(UUID_STR).expect("static uuid is valid"));
        assert_eq!(parsed, expected);
    }

    /// The hex form of the head also survives the three-segment
    /// suffix, mirroring [`accepts_hex_partition_default_suffix`] for
    /// the bare-suffix case.
    #[test]
    fn accepts_three_segment_partition_hex_head() {
        let id = AgentId::new([5; 32]);
        let parsed = parse_agent_id(&format!("{}::inst::sess", id.to_hex()))
            .expect("three-segment partitioned hex should parse");
        assert_eq!(parsed, id);
    }

    /// A garbage head still fails even when the partition has three
    /// segments — the suffix is only metadata, never a fallback.
    #[test]
    fn three_segment_with_invalid_head_returns_err() {
        let err = parse_agent_id("not-a-uuid::inst::sess")
            .expect_err("garbage prefix in three-segment partition should fail");
        assert!(format!("{err:?}").contains("invalid agent_id"));
    }
}
