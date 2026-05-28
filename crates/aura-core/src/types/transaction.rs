//! Transaction types: the immutable input to the Aura system.

use crate::error::AuraError;
#[allow(deprecated)]
use crate::ids::{AgentId, Hash, TxId};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{ActionResultPayload, ToolExecution, ToolProposal};

/// The type of transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionType {
    /// User-initiated prompt/message
    UserPrompt,
    /// Message from another agent
    AgentMsg,
    /// Scheduled or event-based trigger
    Trigger,
    /// Result from a previously executed action
    ActionResult,
    /// System-generated transaction
    System,
    /// Session/context reset marker
    SessionStart,
    /// Tool proposal from the reasoner (LLM suggestion, before policy check)
    ToolProposal,
    /// Tool execution result (after kernel policy decision)
    ToolExecution,
    /// Async process completion (callback from background process)
    ProcessComplete,
    /// LLM call record (model request + response metadata)
    Reasoning,
    /// Phase 10 schema-v2: typed wire variant for the
    /// `SubagentSpawn` audit row that Phase 7a wrote under
    /// `TransactionType::System + JSON-discriminator`. The payload
    /// carries the spawner's override-manifest bytes (no embedded
    /// `"kind"` field — the discriminator now lives in the
    /// transaction header itself).
    SubagentSpawn,
}

/// Sub-categorization for System transactions.
///
/// Enables record scanning to filter by system event category without
/// full payload deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemKind {
    /// Automaton start, stop, pause, resume
    AutomatonLifecycle,
    /// Runtime capability installation snapshot for a session/automaton
    CapabilityInstall,
    /// Domain API mutations: `create_spec`, `delete_spec`, `transition_task`, `save_message`
    DomainMutation,
    /// Login, logout
    AuthChange,
    /// Config changes, schema migrations (future use)
    Infrastructure,
}

/// An immutable transaction input to the system.
///
/// Transactions are the only way state can change in Aura.
/// The `hash` is derived from content + previous tx hash, creating an immutable chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    /// Unique hash derived from content + previous tx hash (blockchain-style chain)
    /// Uses default (zeroed) hash for backwards compatibility with old records.
    #[serde(default, with = "crate::serde_helpers::hex_hash")]
    pub hash: Hash,
    /// Target agent
    pub agent_id: AgentId,
    /// Timestamp in milliseconds since epoch
    pub ts_ms: u64,
    /// Type of transaction
    pub tx_type: TransactionType,
    /// Versioned payload (opaque bytes)
    #[serde(with = "crate::serde_helpers::bytes_serde")]
    pub payload: Bytes,
    /// Optional reference to a related transaction (for callbacks from async processes)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_helpers::option_hex_hash"
    )]
    pub reference_tx_hash: Option<Hash>,
}

impl Transaction {
    fn now_ms() -> u64 {
        crate::time::now_ms()
    }

    /// Create a new transaction with explicit hash (for replay/import).
    #[must_use]
    pub fn new(
        hash: Hash,
        agent_id: AgentId,
        ts_ms: u64,
        tx_type: TransactionType,
        payload: impl Into<Bytes>,
    ) -> Self {
        Self {
            hash,
            agent_id,
            ts_ms,
            tx_type,
            payload: payload.into(),
            reference_tx_hash: None,
        }
    }

    /// Create a new transaction chained to a previous transaction.
    #[must_use]
    pub fn new_chained(
        agent_id: AgentId,
        tx_type: TransactionType,
        payload: impl Into<Bytes>,
        prev_hash: Option<&Hash>,
    ) -> Self {
        let payload = payload.into();
        let hash = Hash::from_content_chained(&payload, prev_hash);
        let ts_ms = Self::now_ms();

        Self {
            hash,
            agent_id,
            ts_ms,
            tx_type,
            payload,
            reference_tx_hash: None,
        }
    }

    /// Create a user prompt transaction.
    #[must_use]
    pub fn user_prompt(agent_id: AgentId, payload: impl Into<Bytes>) -> Self {
        Self::new_chained(agent_id, TransactionType::UserPrompt, payload, None)
    }

    /// Create a user prompt transaction chained to a previous transaction.
    #[must_use]
    pub fn user_prompt_chained(
        agent_id: AgentId,
        payload: impl Into<Bytes>,
        prev_hash: &Hash,
    ) -> Self {
        Self::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            payload,
            Some(prev_hash),
        )
    }

    /// Create an action result transaction.
    #[must_use]
    pub fn action_result(agent_id: AgentId, payload: impl Into<Bytes>) -> Self {
        Self::new_chained(agent_id, TransactionType::ActionResult, payload, None)
    }

    /// Create an action result with a reference to the originating transaction.
    ///
    /// Used for async process completions that need to reference their origin.
    #[must_use]
    pub fn action_result_with_reference(
        agent_id: AgentId,
        payload: impl Into<Bytes>,
        reference_tx_hash: Hash,
        prev_hash: Option<&Hash>,
    ) -> Self {
        let payload = payload.into();
        let hash = Hash::from_content_chained(&payload, prev_hash);
        let ts_ms = Self::now_ms();

        Self {
            hash,
            agent_id,
            ts_ms,
            tx_type: TransactionType::ActionResult,
            payload,
            reference_tx_hash: Some(reference_tx_hash),
        }
    }

    /// Create a session start transaction (context reset marker).
    #[must_use]
    pub fn session_start(agent_id: AgentId) -> Self {
        Self::new_chained(
            agent_id,
            TransactionType::SessionStart,
            Bytes::from_static(b"session_start"),
            None,
        )
    }

    /// Create a tool proposal transaction.
    ///
    /// Records a tool call suggested by the reasoner (LLM) before policy evaluation.
    /// The payload contains the proposed tool call details.
    ///
    /// # Errors
    /// Returns `AuraError::Serialization` if the proposal cannot be serialized.
    pub fn tool_proposal(agent_id: AgentId, proposal: &ToolProposal) -> Result<Self, AuraError> {
        let payload = serde_json::to_vec(proposal)?;
        Ok(Self::new_chained(
            agent_id,
            TransactionType::ToolProposal,
            payload,
            None,
        ))
    }

    /// Create a tool execution transaction.
    ///
    /// Records the kernel's decision and execution result for a tool proposal.
    /// This captures what actually happened after policy evaluation.
    ///
    /// # Errors
    /// Returns `AuraError::Serialization` if the execution cannot be serialized.
    pub fn tool_execution(agent_id: AgentId, execution: &ToolExecution) -> Result<Self, AuraError> {
        let payload = serde_json::to_vec(execution)?;
        Ok(Self::new_chained(
            agent_id,
            TransactionType::ToolExecution,
            payload,
            None,
        ))
    }

    /// Create a process completion transaction.
    ///
    /// Records the result of an async process that completed after the initial
    /// transaction was recorded. Links back to the originating transaction.
    ///
    /// # Errors
    /// Returns `AuraError::Serialization` if the payload cannot be serialized.
    pub fn process_complete(
        agent_id: AgentId,
        payload: &ActionResultPayload,
        reference_tx_hash: Hash,
        prev_hash: Option<&Hash>,
    ) -> Result<Self, AuraError> {
        let payload_bytes = serde_json::to_vec(payload)?;
        let hash = Hash::from_content_chained(&payload_bytes, prev_hash);
        let ts_ms = Self::now_ms();

        Ok(Self {
            hash,
            agent_id,
            ts_ms,
            tx_type: TransactionType::ProcessComplete,
            payload: payload_bytes.into(),
            reference_tx_hash: Some(reference_tx_hash),
        })
    }

    /// Get the transaction hash (legacy compatibility with `tx_id`).
    #[deprecated(note = "use Transaction.hash — tx_id() is a legacy compat accessor")]
    #[allow(deprecated)]
    #[must_use]
    pub const fn tx_id(&self) -> TxId {
        TxId::new(*self.hash.as_bytes())
    }
}
