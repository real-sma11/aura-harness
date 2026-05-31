//! Proposal and decision types from the reasoner and kernel.

use crate::ids::ActionId;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::ActionKind;

/// A proposal from the reasoner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Proposed action kind
    pub action_kind: ActionKind,
    /// Payload for the proposed action
    #[serde(with = "crate::serde_helpers::bytes_serde")]
    pub payload: Bytes,
    /// Optional reasoning for the proposal
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

impl Proposal {
    /// Create a new proposal.
    #[must_use]
    pub fn new(action_kind: ActionKind, payload: impl Into<Bytes>) -> Self {
        Self {
            action_kind,
            payload: payload.into(),
            rationale: None,
        }
    }

    /// Add a rationale to the proposal.
    #[must_use]
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = Some(rationale.into());
        self
    }
}

/// Trace information from the reasoner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    /// Model used for reasoning
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Latency in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Additional metadata
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// A set of proposals from the reasoner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalSet {
    /// List of proposals
    pub proposals: Vec<Proposal>,
    /// Optional trace information
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Trace>,
}

impl ProposalSet {
    /// Create a new empty proposal set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a proposal set with proposals.
    #[must_use]
    pub const fn with_proposals(proposals: Vec<Proposal>) -> Self {
        Self {
            proposals,
            trace: None,
        }
    }

    /// Add trace information.
    #[must_use]
    pub fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// Information about a rejected proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedProposal {
    /// Index of the rejected proposal
    pub proposal_index: u32,
    /// Reason for rejection
    pub reason: String,
}

/// The decision made by the kernel after evaluating proposals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// IDs of accepted actions
    pub accepted_action_ids: Vec<ActionId>,
    /// Information about rejected proposals
    pub rejected: Vec<RejectedProposal>,
}

impl Decision {
    /// Create a new empty decision.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept an action.
    pub fn accept(&mut self, action_id: ActionId) {
        self.accepted_action_ids.push(action_id);
    }

    /// Reject a proposal.
    pub fn reject(&mut self, proposal_index: u32, reason: impl Into<String>) {
        self.rejected.push(RejectedProposal {
            proposal_index,
            reason: reason.into(),
        });
    }
}
