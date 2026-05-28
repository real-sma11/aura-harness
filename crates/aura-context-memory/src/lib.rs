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
//! Phase 6a status (TIGHTENED prompt redo): the upward edges into
//! `aura-agent` — `AgentLoopResult`, `AgentLoopConfig`,
//! `KernelModelGateway`, and the `TurnObserver` trait — still hold.
//! A partial inversion (a layer-neutral `TurnSummary` mirror +
//! injecting `Arc<dyn ModelProvider>` into `MemoryConsolidator` /
//! `LlmRefiner` / `MemoryManager`) was prototyped this phase and
//! reverted because relocating the `MemoryTurnObserver`
//! `TurnObserver` impl plus the
//! `prepare_context(&mut AgentLoopConfig)` site to `aura-agent`
//! requires touching three crates (`aura-context-memory`,
//! `aura-agent`, `aura-runtime::node`) and is incompatible with
//! Phase 6a's audit-tier-focused scope. The inversion is tracked
//! as `Phase 6c — context-memory inversion` in the architecture
//! plan. The advisory `tests/layer_boundary.rs` check continues to
//! WARN about the edge, not fail.

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
mod types;
mod write_pipeline;

#[cfg(test)]
mod test_kernel;

pub use consolidation::{ConsolidationConfig, ConsolidationReport, MemoryConsolidator};
pub use error::MemoryError;
pub use extraction::ConversationTurn;
pub use manager::MemoryManager;
pub use procedures::{compute_skill_relevance, ProcedureConfig, ProcedureExtractor, StepSequence};
pub use refinement::RefinerConfig;
pub use retrieval::{MemoryRetriever, RetrievalConfig};
pub use salience::{estimate_tokens, score_event, score_fact, score_procedure};
pub use store::{MemoryStats, MemoryStore, MemoryStoreApi};
pub use types::{AgentEvent, Fact, FactSource, MemoryCandidate, MemoryPacket, Procedure};
pub use write_pipeline::{MemoryWritePipeline, WriteConfig, WriteReport};
