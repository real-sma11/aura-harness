//! Top-level facade that owns the store, retriever, write pipeline, and consolidator.

use crate::consolidation::{ConsolidationConfig, ConsolidationReport, MemoryConsolidator};
use crate::error::MemoryError;
use crate::extraction::ConversationTurn;
use crate::procedures::{ProcedureConfig, ProcedureExtractor, StepSequence};
use crate::refinement::{LlmRefiner, RefinerConfig};
use crate::retrieval::{MemoryRetriever, RetrievalConfig};
use crate::store::{MemoryStore, MemoryStoreApi};
use crate::types::{MemoryPacket, Procedure};
use crate::write_pipeline::{MemoryWritePipeline, WriteConfig, WriteReport};
use async_trait::async_trait;
use aura_agent::{AgentLoopResult, KernelModelGateway};
use aura_core::AgentId;
use aura_core::ProcedureId;
use rocksdb::{DBWithThreadMode, MultiThreaded};
use std::sync::Arc;

/// Top-level memory facade owning the store, retriever, write pipeline,
/// procedure extractor, and consolidator.
pub struct MemoryManager {
    store: Arc<dyn MemoryStoreApi>,
    retriever: MemoryRetriever,
    pipeline: MemoryWritePipeline,
    procedure_extractor: ProcedureExtractor,
    consolidator: MemoryConsolidator,
}

impl MemoryManager {
    /// Create a new `MemoryManager` backed by a shared `RocksDB` instance.
    ///
    /// `provider` must be a [`KernelModelGateway`] so LLM calls performed
    /// during memory refinement and consolidation are recorded in the
    /// kernel's append-only log (Invariant §3).
    pub fn new(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        provider: Arc<KernelModelGateway>,
        refiner_config: RefinerConfig,
        write_config: WriteConfig,
        retrieval_config: RetrievalConfig,
        consolidation_config: ConsolidationConfig,
        procedure_config: ProcedureConfig,
    ) -> Self {
        let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(db));
        let retriever = MemoryRetriever::new(Arc::clone(&store), retrieval_config);
        let refiner = LlmRefiner::new(Arc::clone(&provider), refiner_config);
        let pipeline = MemoryWritePipeline::new(Arc::clone(&store), refiner, write_config);
        let procedure_extractor = ProcedureExtractor::new(Arc::clone(&store), procedure_config);
        let consolidator =
            MemoryConsolidator::new(Arc::clone(&store), provider, consolidation_config);

        Self {
            store,
            retriever,
            pipeline,
            procedure_extractor,
            consolidator,
        }
    }

    /// Retrieve a memory packet for system prompt injection.
    ///
    /// # Errors
    /// Returns error on store read failure.
    pub async fn retrieve(&self, agent_id: AgentId) -> Result<MemoryPacket, MemoryError> {
        self.retriever.retrieve(agent_id).await
    }

    /// Ingest an agent loop result through the write pipeline.
    ///
    /// # Errors
    /// Returns error on extraction, refinement, or storage failure.
    pub async fn ingest(
        &self,
        agent_id: AgentId,
        result: &AgentLoopResult,
    ) -> Result<WriteReport, MemoryError> {
        let turn = ConversationTurn::from_messages(&result.messages, &result.total_text);
        self.pipeline.ingest(agent_id, result, turn.as_ref()).await
    }

    /// Inject agent memory into the system prompt of an `AgentLoopConfig`.
    ///
    /// Called before the agent loop starts a turn. Strips any existing
    /// `<agent_memory>` block to ensure idempotency, then appends a fresh one.
    pub async fn prepare_context(
        &self,
        agent_id: AgentId,
        config: &mut aura_agent::AgentLoopConfig,
    ) {
        if let Some(idx) = config.system_prompt.find("\n<agent_memory>") {
            config.system_prompt.truncate(idx);
        }

        match self.retrieve(agent_id).await {
            Ok(packet) => {
                let block = packet.format_for_prompt();
                if !block.is_empty() {
                    config.system_prompt.push_str(&block);
                }
            }
            Err(e) => {
                // Phase 5 (error-handling polish): keep this best-effort
                // (memory retrieval is non-critical for the agent loop)
                // but include `agent_id` so operators can correlate
                // prompt-injection misses with the affected agent.
                tracing::warn!(
                    error = %e,
                    agent_id = ?agent_id,
                    "Failed to retrieve memory for prompt injection"
                );
            }
        }
    }

    /// Process an agent loop result through the write pipeline.
    ///
    /// Extracts the last conversation turn from message history and feeds
    /// both heuristic and LLM extraction.
    ///
    /// # Errors
    /// Returns error on extraction, refinement, or storage failure.
    pub async fn process_result(
        &self,
        agent_id: AgentId,
        result: &AgentLoopResult,
    ) -> Result<WriteReport, MemoryError> {
        self.process_result_with_token(agent_id, result, None).await
    }

    /// Like [`process_result`](Self::process_result) but with an explicit
    /// auth token for proxy-mode LLM calls.
    pub async fn process_result_with_token(
        &self,
        agent_id: AgentId,
        result: &AgentLoopResult,
        auth_token: Option<String>,
    ) -> Result<WriteReport, MemoryError> {
        self.process_result_with_context(agent_id, result, auth_token, &[])
            .await
    }

    /// Like [`process_result_with_token`](Self::process_result_with_token) but
    /// also forwards active skill names so the refiner can associate extracted
    /// procedures with their relevant skill.
    pub async fn process_result_with_context(
        &self,
        agent_id: AgentId,
        result: &AgentLoopResult,
        auth_token: Option<String>,
        active_skills: &[String],
    ) -> Result<WriteReport, MemoryError> {
        let turn = ConversationTurn::from_messages(&result.messages, &result.total_text);
        self.pipeline
            .ingest_with_context(agent_id, result, turn.as_ref(), auth_token, active_skills)
            .await
    }

    /// Run post-session consolidation (forget, compress, evolve) for an agent.
    ///
    /// # Errors
    /// Returns error on store I/O or model provider failure.
    pub async fn consolidate(&self, agent_id: AgentId) -> Result<ConsolidationReport, MemoryError> {
        self.consolidator.consolidate(agent_id).await
    }

    /// Extract procedural patterns from a step sequence observed during a turn.
    ///
    /// Delegates to [`ProcedureExtractor::extract_from_steps`].
    ///
    /// # Errors
    ///
    /// Returns an error on store read/write failure.
    pub fn extract_procedures(
        &self,
        agent_id: AgentId,
        sequence: &StepSequence,
    ) -> Result<Option<Procedure>, MemoryError> {
        self.procedure_extractor
            .extract_from_steps(agent_id, sequence)
    }

    /// Match stored procedures to a task description by keyword overlap.
    ///
    /// Delegates to [`ProcedureExtractor::match_procedures`].
    ///
    /// # Errors
    ///
    /// Returns an error on store read failure.
    pub fn match_procedures(
        &self,
        agent_id: AgentId,
        task_text: &str,
    ) -> Result<Vec<Procedure>, MemoryError> {
        self.procedure_extractor
            .match_procedures(agent_id, task_text)
    }

    /// Record feedback for a procedure after execution.
    ///
    /// Delegates to [`ProcedureExtractor::record_feedback`].
    ///
    /// # Errors
    ///
    /// Returns an error on store read/write failure or if the procedure is
    /// not found.
    pub fn record_procedure_feedback(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
        succeeded: bool,
        actual_steps: Option<&[String]>,
    ) -> Result<(), MemoryError> {
        self.procedure_extractor
            .record_feedback(agent_id, procedure_id, succeeded, actual_steps)
    }

    /// Get a reference to the underlying memory store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn MemoryStoreApi> {
        &self.store
    }

    /// Create a `TurnObserver` that feeds completed turns into this manager.
    ///
    /// The `auth_token` is the session JWT needed for proxy-mode LLM calls
    /// (used by the Haiku extraction model). `active_skills` are the skill
    /// names injected into the session so the refiner can tag extracted
    /// procedures with the relevant skill.
    ///
    /// Attach the returned observer to `AgentLoopConfig::observers` so memory
    /// ingestion fires automatically inside the agent loop.
    pub fn turn_observer(
        self: &Arc<Self>,
        agent_id: AgentId,
        auth_token: Option<String>,
    ) -> Arc<dyn aura_agent::TurnObserver> {
        self.turn_observer_with_skills(agent_id, auth_token, Vec::new())
    }

    pub fn turn_observer_with_skills(
        self: &Arc<Self>,
        agent_id: AgentId,
        auth_token: Option<String>,
        active_skills: Vec<String>,
    ) -> Arc<dyn aura_agent::TurnObserver> {
        Arc::new(MemoryTurnObserver {
            manager: Arc::clone(self),
            agent_id,
            auth_token,
            active_skills,
        })
    }
}

/// Adapter that implements [`aura_agent::TurnObserver`] by delegating to
/// [`MemoryManager::process_result`].
struct MemoryTurnObserver {
    manager: Arc<MemoryManager>,
    agent_id: AgentId,
    auth_token: Option<String>,
    active_skills: Vec<String>,
}

#[async_trait]
impl aura_agent::TurnObserver for MemoryTurnObserver {
    async fn on_turn_complete(&self, result: &AgentLoopResult) {
        if let Err(e) = self
            .manager
            .process_result_with_context(
                self.agent_id,
                result,
                self.auth_token.clone(),
                &self.active_skills,
            )
            .await
        {
            // Phase 5 (error-handling polish): the observer is
            // intentionally best-effort — a failed memory write must
            // not abort the conversation — but the warning gains an
            // `agent_id` field so a flapping ingest pipeline is
            // greppable in the structured-log stream.
            tracing::warn!(
                error = %e,
                agent_id = ?self.agent_id,
                "Memory ingestion failed after turn"
            );
        }
    }
}
