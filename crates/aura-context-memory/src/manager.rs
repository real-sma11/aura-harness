//! Top-level facade that owns the store, retriever, write pipeline, and consolidator.

use crate::consolidation::{ConsolidationConfig, ConsolidationReport, MemoryConsolidator};
use crate::error::MemoryError;
use crate::extraction::ConversationTurn;
use crate::procedures::{ProcedureConfig, ProcedureExtractor, StepSequence};
use crate::refinement::{LlmRefiner, RefinerConfig};
use crate::retrieval::{MemoryRetriever, RetrievalConfig};
use crate::store::{MemoryStore, MemoryStoreApi};
use crate::turn_summary::TurnSummary;
use crate::types::{MemoryPacket, Procedure};
use crate::write_pipeline::{MemoryWritePipeline, WriteConfig, WriteReport};
use aura_core_types::AgentId;
use aura_core_types::ProcedureId;
use aura_model_reasoner::ModelProvider;
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
    /// `provider` must be a recording-capable [`ModelProvider`] —
    /// production wiring passes `aura_agent::KernelModelGateway`, which
    /// routes every completion through the kernel's append-only log so
    /// Invariant §3 ("Every LLM Call Is Recorded") holds. The context
    /// layer accepts the abstract trait object to keep this crate free
    /// of any upward edge into `aura-agent`; the runtime composition
    /// root (`aura_runtime::node`) is the single place that knows how
    /// to construct a recording provider.
    pub fn new(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        provider: Arc<dyn ModelProvider + Send + Sync>,
        refiner_config: RefinerConfig,
        write_config: WriteConfig,
        retrieval_config: RetrievalConfig,
        consolidation_config: ConsolidationConfig,
        procedure_config: ProcedureConfig,
    ) -> Self {
        Self::with_cipher(
            db,
            None,
            provider,
            refiner_config,
            write_config,
            retrieval_config,
            consolidation_config,
            procedure_config,
        )
    }

    /// Like [`Self::new`], but with optional sealed (encrypted-at-rest)
    /// memory values (Swarm TEE phase 5). `cipher: None` is exactly the
    /// legacy plaintext behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn with_cipher(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        cipher: Option<Arc<aura_store_db::SealCipher>>,
        provider: Arc<dyn ModelProvider + Send + Sync>,
        refiner_config: RefinerConfig,
        write_config: WriteConfig,
        retrieval_config: RetrievalConfig,
        consolidation_config: ConsolidationConfig,
        procedure_config: ProcedureConfig,
    ) -> Self {
        let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::with_cipher(db, cipher));
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

    /// Ingest a finished turn through the write pipeline.
    ///
    /// # Errors
    /// Returns error on extraction, refinement, or storage failure.
    pub async fn ingest(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
    ) -> Result<WriteReport, MemoryError> {
        let turn = ConversationTurn::from_messages(&summary.messages, &summary.total_text);
        self.pipeline.ingest(agent_id, summary, turn.as_ref()).await
    }

    /// Inject agent memory into the given system prompt string.
    ///
    /// Called before the agent loop starts a turn. Strips any existing
    /// `<agent_memory>` block to ensure idempotency, then appends a
    /// fresh one. Operating on `&mut String` (rather than `&mut
    /// AgentLoopConfig`) keeps this crate free of any upward edge into
    /// `aura-agent` — callers in `aura-runtime` pass
    /// `&mut config.system_prompt` explicitly.
    pub async fn prepare_context(&self, agent_id: AgentId, system_prompt: &mut String) {
        if let Some(idx) = system_prompt.find("\n<agent_memory>") {
            system_prompt.truncate(idx);
        }

        match self.retrieve(agent_id).await {
            Ok(packet) => {
                let block = packet.format_for_prompt();
                if !block.is_empty() {
                    system_prompt.push_str(&block);
                }
            }
            Err(e) => {
                // Best-effort: memory retrieval is non-critical for the
                // agent loop. Include `agent_id` so operators can
                // correlate prompt-injection misses with the affected
                // agent.
                tracing::warn!(
                    error = %e,
                    agent_id = ?agent_id,
                    "Failed to retrieve memory for prompt injection"
                );
            }
        }
    }

    /// Process a turn summary through the write pipeline.
    ///
    /// Extracts the last conversation turn from message history and feeds
    /// both heuristic and LLM extraction.
    ///
    /// # Errors
    /// Returns error on extraction, refinement, or storage failure.
    pub async fn process_result(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
    ) -> Result<WriteReport, MemoryError> {
        self.process_result_with_token(agent_id, summary, None)
            .await
    }

    /// Like [`process_result`](Self::process_result) but with an explicit
    /// auth token for proxy-mode LLM calls.
    pub async fn process_result_with_token(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        auth_token: Option<String>,
    ) -> Result<WriteReport, MemoryError> {
        self.process_result_with_context(agent_id, summary, auth_token, &[])
            .await
    }

    /// Like [`process_result_with_token`](Self::process_result_with_token) but
    /// also forwards active skill names so the refiner can associate extracted
    /// procedures with their relevant skill.
    pub async fn process_result_with_context(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        auth_token: Option<String>,
        active_skills: &[String],
    ) -> Result<WriteReport, MemoryError> {
        let turn = ConversationTurn::from_messages(&summary.messages, &summary.total_text);
        self.pipeline
            .ingest_with_context(agent_id, summary, turn.as_ref(), auth_token, active_skills)
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
}
