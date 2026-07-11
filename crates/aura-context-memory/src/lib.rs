//! # aura-context-memory
//!
//! Per-agent memory system for Aura.
//!
//! Provides fact storage, episodic event logging, procedural pattern detection,
//! a two-stage write pipeline (heuristic extraction + LLM refinement), and
//! deterministic retrieval for system prompt injection.
//!
//! Layer: context
//!
//! Phase 6c (context-memory inversion) completed the inversion that was
//! prototyped and reverted during Phase 6a: this crate no longer depends
//! on `aura-agent`. The pipeline now consumes the layer-neutral
//! [`TurnSummary`] mirror, and the [`MemoryManager`] / [`LlmRefiner`] /
//! [`MemoryConsolidator`] take `Arc<dyn aura_model_reasoner::ModelProvider>`
//! directly. The runtime-side adapter that bridges the agent loop's
//! `AgentLoopResult` + `TurnObserver` to this crate lives in
//! `aura_runtime::memory_observer`. `tests/layer_boundary.rs` now
//! fails on any `aura-context-memory -> aura-agent` edge.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
#![allow(clippy::option_if_let_else)]

mod consolidation;
mod error;
mod extraction;
mod manager;
mod procedures;
mod refinement;
mod retrieval;
mod salience;
mod store;
mod turn_summary;
mod types;
mod write_pipeline;

#[cfg(test)]
mod retriever_tests;

pub use consolidation::{ConsolidationConfig, ConsolidationReport, MemoryConsolidator};
pub use error::MemoryError;
pub use extraction::ConversationTurn;
pub use manager::MemoryManager;
pub use procedures::{compute_skill_relevance, ProcedureConfig, ProcedureExtractor, StepSequence};
pub use refinement::{LlmRefiner, RefinementRequestContext, RefinerConfig};
pub use retrieval::{MemoryQueryContext, MemoryRetriever, RetrievalConfig};
pub use salience::{estimate_tokens, score_event, score_fact, score_procedure};
pub use store::{MemoryStats, MemoryStore, MemoryStoreApi};
pub use turn_summary::TurnSummary;
pub use types::{
    AgentContinuityConfig, AgentEvent, Fact, FactSource, MemoryCandidate, MemoryContinuity,
    MemoryPacket, MemoryProvenance, MemoryRetrievalMode, MemoryRetrievalTrace, MemoryScope,
    MemorySelection, MemorySensitivity, MemoryStatus, MemoryWritePolicy, Procedure,
};
pub use write_pipeline::{MemoryWritePipeline, WriteConfig, WriteReport};
