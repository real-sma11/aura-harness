//! Request types for the reasoner.
//!
//! NOTE: `ProposeRequest`, `RecordSummary`, and `ProposeLimits` are kernel integration
//! DTOs that couple this crate to aura-core domain types. Consider migrating them to
//! aura-core or a shared protocol crate in a future refactor to keep aura-reasoner
//! focused on LLM client concerns.

use aura_core::{ActionKind, AgentId, Transaction};
use serde::{Deserialize, Serialize};

/// A summary of a record entry for context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordSummary {
    /// Sequence number
    pub seq: u64,
    /// Transaction kind
    pub tx_kind: String,
    /// Action kinds that were taken
    pub action_kinds: Vec<ActionKind>,
    /// Truncated payload for context
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_summary: Option<String>,
}

/// Request to the reasoner for proposals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeRequest {
    /// Agent making the request
    pub agent_id: AgentId,
    /// Current transaction to process
    pub tx: Transaction,
    /// Recent record entries for context
    pub record_window: Vec<RecordSummary>,
    /// Limits for the response
    pub limits: ProposeLimits,
}

/// Limits for proposal generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeLimits {
    /// Maximum number of proposals to return
    pub max_proposals: u32,
}

impl Default for ProposeLimits {
    fn default() -> Self {
        Self { max_proposals: 8 }
    }
}

impl ProposeRequest {
    /// Create a new propose request.
    #[must_use]
    pub fn new(agent_id: AgentId, tx: Transaction) -> Self {
        Self {
            agent_id,
            tx,
            record_window: Vec::new(),
            limits: ProposeLimits::default(),
        }
    }

    /// Add record window context.
    #[must_use]
    pub fn with_record_window(mut self, window: Vec<RecordSummary>) -> Self {
        self.record_window = window;
        self
    }

    /// Set limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: ProposeLimits) -> Self {
        self.limits = limits;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{Hash, TransactionType};
    use bytes::Bytes;

    fn create_test_tx(agent_id: AgentId) -> Transaction {
        Transaction::new(
            Hash::from_content(b"test"),
            agent_id,
            1000,
            TransactionType::UserPrompt,
            Bytes::from("test message"),
        )
    }

    #[test]
    fn test_propose_limits_default() {
        let limits = ProposeLimits::default();
        assert_eq!(limits.max_proposals, 8);
    }

    #[test]
    fn test_record_summary_serialization() {
        let summary = RecordSummary {
            seq: 42,
            tx_kind: "UserPrompt".to_string(),
            action_kinds: vec![ActionKind::Reason, ActionKind::Delegate],
            payload_summary: Some("Hello, world!".to_string()),
        };

        let json = serde_json::to_string(&summary).unwrap();
        let parsed: RecordSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.tx_kind, "UserPrompt");
        assert_eq!(parsed.action_kinds.len(), 2);
        assert_eq!(parsed.payload_summary, Some("Hello, world!".to_string()));
    }

    #[test]
    fn test_record_summary_without_payload() {
        let summary = RecordSummary {
            seq: 1,
            tx_kind: "System".to_string(),
            action_kinds: vec![],
            payload_summary: None,
        };

        let json = serde_json::to_string(&summary).unwrap();
        // payload_summary should be skipped when None
        assert!(!json.contains("payload_summary"));

        let parsed: RecordSummary = serde_json::from_str(&json).unwrap();
        assert!(parsed.payload_summary.is_none());
    }

    #[test]
    fn test_propose_request_new() {
        let agent_id = AgentId::generate();
        let tx = create_test_tx(agent_id);

        let request = ProposeRequest::new(agent_id, tx.clone());

        assert_eq!(request.agent_id, agent_id);
        assert_eq!(request.tx.hash, tx.hash);
        assert!(request.record_window.is_empty());
        assert_eq!(request.limits.max_proposals, 8);
    }

    #[test]
    fn test_propose_request_with_record_window() {
        let agent_id = AgentId::generate();
        let tx = create_test_tx(agent_id);

        let window = vec![
            RecordSummary {
                seq: 1,
                tx_kind: "UserPrompt".to_string(),
                action_kinds: vec![],
                payload_summary: Some("first".to_string()),
            },
            RecordSummary {
                seq: 2,
                tx_kind: "AgentMsg".to_string(),
                action_kinds: vec![ActionKind::Reason],
                payload_summary: Some("second".to_string()),
            },
        ];

        let request = ProposeRequest::new(agent_id, tx).with_record_window(window);

        assert_eq!(request.record_window.len(), 2);
        assert_eq!(request.record_window[0].seq, 1);
        assert_eq!(request.record_window[1].seq, 2);
    }

    #[test]
    fn test_propose_request_with_limits() {
        let agent_id = AgentId::generate();
        let tx = create_test_tx(agent_id);

        let limits = ProposeLimits { max_proposals: 16 };

        let request = ProposeRequest::new(agent_id, tx).with_limits(limits);

        assert_eq!(request.limits.max_proposals, 16);
    }

    #[test]
    fn test_propose_request_serialization() {
        let agent_id = AgentId::generate();
        let tx = create_test_tx(agent_id);

        let request = ProposeRequest::new(agent_id, tx).with_record_window(vec![RecordSummary {
            seq: 1,
            tx_kind: "UserPrompt".to_string(),
            action_kinds: vec![],
            payload_summary: None,
        }]);

        let json = serde_json::to_string(&request).unwrap();
        let parsed: ProposeRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.agent_id, agent_id);
        assert_eq!(parsed.record_window.len(), 1);
    }

    #[test]
    fn test_propose_request_builder_chain() {
        let agent_id = AgentId::generate();
        let tx = create_test_tx(agent_id);

        let request = ProposeRequest::new(agent_id, tx)
            .with_record_window(vec![])
            .with_limits(ProposeLimits { max_proposals: 4 });

        assert!(request.record_window.is_empty());
        assert_eq!(request.limits.max_proposals, 4);
    }
}
