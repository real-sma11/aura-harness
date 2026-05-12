//! Shared identifier parsing helpers for router handlers.
//!
//! Phase 1 (refactor) extracts the duplicated `parse_agent_id` helpers
//! from `memory.rs`, `skills.rs`, `tool_permissions.rs`, and `tx.rs`
//! into one canonical implementation. The function accepts both UUID
//! strings (matching the memory + skills surface) and the 32-byte hex
//! form (matching the tx + tool-permissions surface) so every router
//! endpoint speaks the same agent-id grammar.

use super::errors::ApiError;
use aura_core::AgentId;

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
///   [`crate::session::Session::memory_agent_id`]).
///
/// Errors return [`ApiError::bad_request`] with `400 Bad Request` and a
/// JSON body `{ "error": "invalid agent_id: <reason>" }`.
pub(crate) fn parse_agent_id(s: &str) -> Result<AgentId, ApiError> {
    let head = s.split_once("::").map(|(h, _)| h).unwrap_or(s);
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
}
