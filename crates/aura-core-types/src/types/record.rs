//! Record entry types: the append-only agent log.

use serde::{Deserialize, Serialize};

use super::{Action, ContextHash, Decision, Effect, ProposalSet, Transaction};

/// Current kernel version for record entries.
pub const KERNEL_VERSION: u32 = 1;

/// A single entry in the agent's record (append-only log).
///
/// One `RecordEntry` is created for each processed `Transaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordEntry {
    /// Sequence number (strictly ordered per agent)
    pub seq: u64,
    /// The transaction that was processed
    pub tx: Transaction,
    /// Kernel version that processed this entry
    pub kernel_version: u32,
    /// Hash of deterministic inputs used to decide
    pub context_hash: ContextHash,
    /// Proposals from the reasoner (recorded verbatim)
    pub proposals: ProposalSet,
    /// Decision made by the kernel
    pub decision: Decision,
    /// Authorized actions
    pub actions: Vec<Action>,
    /// Effects from executing actions
    pub effects: Vec<Effect>,
}

impl RecordEntry {
    /// Create a new record entry builder.
    #[must_use]
    pub fn builder(seq: u64, tx: Transaction) -> RecordEntryBuilder {
        RecordEntryBuilder::new(seq, tx)
    }
}

/// Builder for `RecordEntry`.
pub struct RecordEntryBuilder {
    seq: u64,
    tx: Transaction,
    context_hash: ContextHash,
    proposals: ProposalSet,
    decision: Decision,
    actions: Vec<Action>,
    effects: Vec<Effect>,
}

impl RecordEntryBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new(seq: u64, tx: Transaction) -> Self {
        Self {
            seq,
            tx,
            context_hash: ContextHash::zero(),
            proposals: ProposalSet::new(),
            decision: Decision::new(),
            actions: Vec::new(),
            effects: Vec::new(),
        }
    }

    /// Set the context hash.
    #[must_use]
    pub fn context_hash(mut self, hash: impl Into<ContextHash>) -> Self {
        self.context_hash = hash.into();
        self
    }

    /// Set the proposals.
    #[must_use]
    pub fn proposals(mut self, proposals: ProposalSet) -> Self {
        self.proposals = proposals;
        self
    }

    /// Set the decision.
    #[must_use]
    pub fn decision(mut self, decision: Decision) -> Self {
        self.decision = decision;
        self
    }

    /// Set the actions.
    #[must_use]
    pub fn actions(mut self, actions: Vec<Action>) -> Self {
        self.actions = actions;
        self
    }

    /// Set the effects.
    #[must_use]
    pub fn effects(mut self, effects: Vec<Effect>) -> Self {
        self.effects = effects;
        self
    }

    /// Build the record entry.
    #[must_use]
    pub fn build(self) -> RecordEntry {
        RecordEntry {
            seq: self.seq,
            tx: self.tx,
            kernel_version: KERNEL_VERSION,
            context_hash: self.context_hash,
            proposals: self.proposals,
            decision: self.decision,
            actions: self.actions,
            effects: self.effects,
        }
    }
}
