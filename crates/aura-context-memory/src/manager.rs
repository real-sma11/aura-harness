//! Top-level facade that owns the store, retriever, write pipeline, and consolidator.

use crate::consolidation::{ConsolidationConfig, ConsolidationReport, MemoryConsolidator};
use crate::error::MemoryError;
use crate::extraction::ConversationTurn;
use crate::procedures::{ProcedureConfig, ProcedureExtractor, StepSequence};
use crate::refinement::{LlmRefiner, RefinementRequestContext, RefinerConfig};
use crate::retrieval::{MemoryQueryContext, MemoryRetriever, RetrievalConfig};
use crate::store::{MemoryStore, MemoryStoreApi};
use crate::turn_summary::TurnSummary;
use crate::types::{
    AgentContinuityConfig, MemoryPacket, MemoryRetrievalTrace, MemoryStatus, MemoryWritePolicy,
    Procedure,
};
use crate::write_pipeline::{MemoryWritePipeline, WriteConfig, WriteReport};
use aura_core_types::AgentId;
use aura_core_types::ProcedureId;
use aura_model_reasoner::ModelProvider;
use rocksdb::{DBWithThreadMode, MultiThreaded};
use std::sync::Arc;
use std::sync::Mutex;
use std::{collections::HashMap, time::Duration};

/// Top-level memory facade owning the store, retriever, write pipeline,
/// procedure extractor, and consolidator.
pub struct MemoryManager {
    store: Arc<dyn MemoryStoreApi>,
    retriever: MemoryRetriever,
    pipeline: MemoryWritePipeline,
    procedure_extractor: ProcedureExtractor,
    consolidator: MemoryConsolidator,
    latest_traces: Mutex<HashMap<AgentId, MemoryRetrievalTrace>>,
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
            latest_traces: Mutex::new(HashMap::new()),
        }
    }

    /// Retrieve a memory packet for system prompt injection.
    ///
    /// # Errors
    /// Returns error on store read failure.
    pub async fn retrieve(&self, agent_id: AgentId) -> Result<MemoryPacket, MemoryError> {
        self.retriever.retrieve(agent_id).await
    }

    /// Load persisted Agent Continuity controls for an agent.
    pub async fn continuity_config(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentContinuityConfig, MemoryError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.get_continuity_config(agent_id))
            .await
            .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))?
    }

    /// Persist Agent Continuity controls for an agent.
    pub async fn save_continuity_config(
        &self,
        agent_id: AgentId,
        config: AgentContinuityConfig,
    ) -> Result<AgentContinuityConfig, MemoryError> {
        let store = Arc::clone(&self.store);
        let saved = config.clone();
        tokio::task::spawn_blocking(move || store.put_continuity_config(agent_id, &config))
            .await
            .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??;
        Ok(saved)
    }

    #[must_use]
    pub fn latest_retrieval_trace(&self, agent_id: AgentId) -> Option<MemoryRetrievalTrace> {
        self.latest_traces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&agent_id)
            .cloned()
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
        self.prepare_context_with_query(agent_id, system_prompt, MemoryQueryContext::default())
            .await;
    }

    /// Query-aware context preparation used by interactive turns.
    pub async fn prepare_context_with_query(
        &self,
        agent_id: AgentId,
        system_prompt: &mut String,
        mut query: MemoryQueryContext,
    ) {
        if let Some(idx) = system_prompt.find("\n<agent_memory>") {
            system_prompt.truncate(idx);
        }

        let continuity = match self.continuity_config(agent_id).await {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, ?agent_id, "Failed to load Agent Continuity config");
                AgentContinuityConfig::default()
            }
        };
        if !continuity.use_memory {
            return;
        }
        query.allow_user_scope = continuity.allow_user_scope;
        query.allow_workspace_scope = continuity.allow_workspace_scope;

        match self
            .retriever
            .retrieve_with_query(agent_id, query, continuity.retrieval_mode)
            .await
        {
            Ok(packet) => {
                if let Some(trace) = packet.trace.clone() {
                    self.latest_traces
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(agent_id, trace);
                }
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
        self.process_result_with_source(agent_id, summary, auth_token, active_skills, None)
            .await
    }

    pub async fn process_result_with_source(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        auth_token: Option<String>,
        active_skills: &[String],
        source_session_id: Option<&str>,
    ) -> Result<WriteReport, MemoryError> {
        self.process_result_with_source_and_request_context(
            agent_id,
            summary,
            RefinementRequestContext {
                auth_token,
                ..Default::default()
            },
            active_skills,
            source_session_id,
        )
        .await
    }

    pub async fn process_result_with_source_and_request_context(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        request_context: RefinementRequestContext,
        active_skills: &[String],
        source_session_id: Option<&str>,
    ) -> Result<WriteReport, MemoryError> {
        let continuity = self.continuity_config(agent_id).await?;
        if !continuity.generate_memory || continuity.write_policy == MemoryWritePolicy::ExplicitOnly
        {
            return Ok(WriteReport::default());
        }
        let initial_status = if continuity.write_policy == MemoryWritePolicy::Approval {
            MemoryStatus::Pending
        } else {
            MemoryStatus::Active
        };
        let turn = ConversationTurn::from_messages(&summary.messages, &summary.total_text);
        self.pipeline
            .ingest_with_provenance_and_request_context(
                agent_id,
                summary,
                turn.as_ref(),
                request_context,
                active_skills,
                source_session_id,
                initial_status,
            )
            .await
    }

    /// Enqueue memory extraction after a turn without extending response
    /// completion latency. The task is best-effort and bounded by a timeout.
    pub fn process_result_in_background(
        self: &Arc<Self>,
        agent_id: AgentId,
        summary: TurnSummary,
        auth_token: Option<String>,
        active_skills: Vec<String>,
        source_session_id: Option<String>,
    ) {
        self.process_result_in_background_with_request_context(
            agent_id,
            summary,
            RefinementRequestContext {
                auth_token,
                ..Default::default()
            },
            active_skills,
            source_session_id,
        );
    }

    pub fn process_result_in_background_with_request_context(
        self: &Arc<Self>,
        agent_id: AgentId,
        summary: TurnSummary,
        request_context: RefinementRequestContext,
        active_skills: Vec<String>,
        source_session_id: Option<String>,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(45),
                manager.process_result_with_source_and_request_context(
                    agent_id,
                    &summary,
                    request_context,
                    &active_skills,
                    source_session_id.as_deref(),
                ),
            )
            .await;
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, ?agent_id, "Background memory ingestion failed")
                }
                Err(_) => tracing::warn!(?agent_id, "Background memory ingestion timed out"),
            }
        });
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
