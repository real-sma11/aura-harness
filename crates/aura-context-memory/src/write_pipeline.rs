//! Orchestrates Stage 1 (heuristic) -> Stage 2 (LLM extraction + refinement) -> write.

use crate::error::MemoryError;
use crate::extraction::{ConversationTurn, HeuristicExtractor};
use crate::refinement::LlmRefiner;
use crate::store::MemoryStoreApi;
use crate::turn_summary::TurnSummary;
use crate::types::{
    AgentEvent, CandidateType, Fact, FactSource, MemoryContinuity, MemoryProvenance,
    MemorySensitivity, MemoryStatus, Procedure, RefinedCandidate,
};
use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

pub struct MemoryWritePipeline {
    store: Arc<dyn MemoryStoreApi>,
    extractor: HeuristicExtractor,
    refiner: LlmRefiner,
    config: WriteConfig,
}

#[derive(Debug, Clone)]
pub struct WriteConfig {
    pub confidence_floor: f32,
    pub max_facts_per_agent: usize,
    pub max_events_per_agent: usize,
    pub max_procedures_per_agent: usize,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.5,
            max_facts_per_agent: 100,
            max_events_per_agent: 500,
            max_procedures_per_agent: 50,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WriteReport {
    pub candidates_extracted: usize,
    pub candidates_refined: usize,
    pub facts_written: usize,
    pub facts_updated: usize,
    pub events_written: usize,
    pub procedures_written: usize,
    pub candidates_dropped: usize,
    pub sensitive_dropped: usize,
}

impl MemoryWritePipeline {
    #[must_use]
    pub fn new(store: Arc<dyn MemoryStoreApi>, refiner: LlmRefiner, config: WriteConfig) -> Self {
        Self {
            store,
            extractor: HeuristicExtractor,
            refiner,
            config,
        }
    }

    /// Ingest a [`TurnSummary`] through the pipeline.
    ///
    /// Stage 1: free heuristic extraction on assistant text.
    /// Stage 2: LLM call (Haiku) that sees the full conversation turn and
    ///          refines heuristic candidates + extracts new facts.
    ///
    /// # Errors
    /// Returns error on extraction, refinement, or storage failure.
    pub async fn ingest(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        turn: Option<&ConversationTurn>,
    ) -> Result<WriteReport, MemoryError> {
        self.ingest_with_token(agent_id, summary, turn, None).await
    }

    pub async fn ingest_with_token(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        turn: Option<&ConversationTurn>,
        auth_token: Option<String>,
    ) -> Result<WriteReport, MemoryError> {
        self.ingest_with_context(agent_id, summary, turn, auth_token, &[])
            .await
    }

    pub async fn ingest_with_context(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        turn: Option<&ConversationTurn>,
        auth_token: Option<String>,
        active_skills: &[String],
    ) -> Result<WriteReport, MemoryError> {
        self.ingest_with_provenance(
            agent_id,
            summary,
            turn,
            auth_token,
            active_skills,
            None,
            MemoryStatus::Active,
        )
        .await
    }

    pub async fn ingest_with_provenance(
        &self,
        agent_id: AgentId,
        summary: &TurnSummary,
        turn: Option<&ConversationTurn>,
        auth_token: Option<String>,
        active_skills: &[String],
        source_session_id: Option<&str>,
        initial_status: MemoryStatus,
    ) -> Result<WriteReport, MemoryError> {
        let mut report = WriteReport::default();

        // Stage 1: Heuristic extraction (free, no LLM)
        let candidates = self.extractor.extract(summary);
        report.candidates_extracted = candidates.len();

        if candidates.is_empty() && turn.is_none() {
            debug!("No memory candidates and no conversation turn, skipping");
            return Ok(report);
        }

        // Stage 2: LLM extraction + refinement in one call
        let refined = self
            .refiner
            .extract_and_refine_with_skills(candidates, turn, auth_token, active_skills)
            .await?;
        report.candidates_refined = refined.len();

        for candidate in &refined {
            if candidate_is_sensitive(candidate) {
                report.sensitive_dropped += 1;
                continue;
            }
            if !candidate.keep {
                report.candidates_dropped += 1;
                continue;
            }
            if candidate.confidence < self.config.confidence_floor {
                report.candidates_dropped += 1;
                continue;
            }

            match candidate.candidate_type {
                CandidateType::Fact => {
                    self.write_fact(
                        agent_id,
                        candidate,
                        &mut report,
                        source_session_id,
                        initial_status,
                    )?;
                }
                CandidateType::Event => {
                    self.write_event(
                        agent_id,
                        candidate,
                        &mut report,
                        source_session_id,
                        initial_status,
                    )?;
                }
                CandidateType::Procedure => {
                    self.write_procedure(
                        agent_id,
                        candidate,
                        &mut report,
                        source_session_id,
                        initial_status,
                    )?;
                }
            }
        }

        self.enforce_capacity(agent_id)?;

        info!(
            extracted = report.candidates_extracted,
            refined = report.candidates_refined,
            facts = report.facts_written,
            updated = report.facts_updated,
            events = report.events_written,
            procedures = report.procedures_written,
            dropped = report.candidates_dropped,
            sensitive_dropped = report.sensitive_dropped,
            "Memory write pipeline complete"
        );

        Ok(report)
    }

    fn write_fact(
        &self,
        agent_id: AgentId,
        candidate: &RefinedCandidate,
        report: &mut WriteReport,
        source_session_id: Option<&str>,
        initial_status: MemoryStatus,
    ) -> Result<(), MemoryError> {
        let now = Utc::now();

        let existing = if initial_status == MemoryStatus::Active {
            self.store
                .get_fact_by_key(agent_id, &candidate.key)
                .ok()
                .flatten()
        } else {
            None
        };
        if let Some(mut existing) = existing {
            if existing.value != candidate.value {
                let replacement_id = FactId::generate();
                existing.continuity.status = MemoryStatus::Superseded;
                existing.continuity.superseded_by = Some(replacement_id.to_hex());
                existing.updated_at = now;
                self.store.put_fact(&existing)?;

                let replacement = Fact {
                    fact_id: replacement_id,
                    agent_id,
                    key: candidate.key.clone(),
                    value: candidate.value.clone(),
                    confidence: candidate.confidence,
                    source: FactSource::Extracted,
                    importance: candidate.importance,
                    access_count: 0,
                    last_accessed: now,
                    created_at: now,
                    updated_at: now,
                    continuity: continuity_for_candidate(
                        candidate,
                        turn_excerpt(candidate),
                        self.refiner.model_name(),
                        source_session_id,
                        initial_status,
                    ),
                };
                self.store.put_fact(&replacement)?;
                report.facts_updated += 1;
                report.facts_written += 1;
            } else {
                existing.confidence = candidate.confidence;
                existing.importance = candidate.importance;
                existing.updated_at = now;
                self.store.put_fact(&existing)?;
                report.facts_updated += 1;
            }
        } else {
            let fact = Fact {
                fact_id: FactId::generate(),
                agent_id,
                key: candidate.key.clone(),
                value: candidate.value.clone(),
                confidence: candidate.confidence,
                source: FactSource::Extracted,
                importance: candidate.importance,
                access_count: 0,
                last_accessed: now,
                created_at: now,
                updated_at: now,
                continuity: continuity_for_candidate(
                    candidate,
                    turn_excerpt(candidate),
                    self.refiner.model_name(),
                    source_session_id,
                    initial_status,
                ),
            };
            self.store.put_fact(&fact)?;
            report.facts_written += 1;
        }

        Ok(())
    }

    fn write_event(
        &self,
        agent_id: AgentId,
        candidate: &RefinedCandidate,
        report: &mut WriteReport,
        source_session_id: Option<&str>,
        initial_status: MemoryStatus,
    ) -> Result<(), MemoryError> {
        let now = Utc::now();

        let event = AgentEvent {
            event_id: AgentEventId::generate(),
            agent_id,
            event_type: "task_run".to_string(),
            summary: candidate.summary.clone().unwrap_or_default(),
            metadata: candidate.value.clone(),
            importance: candidate.importance,
            access_count: 0,
            last_accessed: now,
            timestamp: now,
            continuity: continuity_for_candidate(
                candidate,
                None,
                self.refiner.model_name(),
                source_session_id,
                initial_status,
            ),
        };
        self.store.put_event(&event)?;
        report.events_written += 1;

        Ok(())
    }

    fn write_procedure(
        &self,
        agent_id: AgentId,
        candidate: &RefinedCandidate,
        report: &mut WriteReport,
        source_session_id: Option<&str>,
        initial_status: MemoryStatus,
    ) -> Result<(), MemoryError> {
        let now = Utc::now();
        let trigger = candidate
            .trigger
            .clone()
            .unwrap_or_else(|| candidate.summary.clone().unwrap_or_default());
        let steps = candidate.steps.clone().unwrap_or_default();

        // Check for an existing procedure with the same name and update it.
        let existing = self.store.list_procedures(agent_id)?;
        let existing_active = if initial_status == MemoryStatus::Active {
            existing
                .into_iter()
                .find(|p| p.name == candidate.key && p.continuity.status == MemoryStatus::Active)
        } else {
            None
        };
        if let Some(mut proc) = existing_active {
            proc.trigger = trigger;
            proc.steps = steps;
            proc.skill_name = candidate.skill_name.clone();
            proc.updated_at = now;
            self.store.put_procedure(&proc)?;
            report.procedures_written += 1;
            return Ok(());
        }

        let procedure = Procedure {
            procedure_id: ProcedureId::generate(),
            agent_id,
            name: candidate.key.clone(),
            trigger,
            steps,
            context_constraints: serde_json::Value::Null,
            success_rate: 1.0,
            execution_count: 0,
            last_used: now,
            created_at: now,
            updated_at: now,
            skill_name: candidate.skill_name.clone(),
            skill_relevance: candidate.skill_name.as_ref().map(|_| 0.8),
            continuity: continuity_for_candidate(
                candidate,
                None,
                self.refiner.model_name(),
                source_session_id,
                initial_status,
            ),
        };
        self.store.put_procedure(&procedure)?;
        report.procedures_written += 1;

        Ok(())
    }

    fn enforce_capacity(&self, agent_id: AgentId) -> Result<(), MemoryError> {
        let mut facts = self.store.list_facts(agent_id)?;
        if facts.len() > self.config.max_facts_per_agent {
            facts.sort_by(|a, b| {
                a.importance
                    .partial_cmp(&b.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let to_remove = facts.len() - self.config.max_facts_per_agent;
            for fact in facts.iter().take(to_remove) {
                self.store.delete_fact(agent_id, fact.fact_id)?;
            }
        }

        let overflow_buffer = 100;
        let events = self
            .store
            .list_events(agent_id, self.config.max_events_per_agent + overflow_buffer)?;
        if events.len() > self.config.max_events_per_agent {
            for event in events.iter().skip(self.config.max_events_per_agent) {
                self.store.delete_event(agent_id, event.event_id)?;
            }
        }

        let mut procs = self.store.list_procedures(agent_id)?;
        #[allow(clippy::cast_precision_loss)]
        if procs.len() > self.config.max_procedures_per_agent {
            procs.sort_by(|a, b| {
                let score_a = a.success_rate * a.execution_count as f32;
                let score_b = b.success_rate * b.execution_count as f32;
                score_a
                    .partial_cmp(&score_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let to_remove = procs.len() - self.config.max_procedures_per_agent;
            for proc in procs.iter().take(to_remove) {
                self.store.delete_procedure(agent_id, proc.procedure_id)?;
            }
        }

        Ok(())
    }
}

fn continuity_for_candidate(
    candidate: &RefinedCandidate,
    excerpt: Option<String>,
    extractor_model: &str,
    source_session_id: Option<&str>,
    initial_status: MemoryStatus,
) -> MemoryContinuity {
    MemoryContinuity {
        status: initial_status,
        sensitivity: if candidate_is_sensitive(candidate) {
            MemorySensitivity::Sensitive
        } else {
            MemorySensitivity::Normal
        },
        provenance: MemoryProvenance {
            session_id: source_session_id.map(str::to_string),
            excerpt,
            extractor_model: Some(extractor_model.to_string()),
            ..MemoryProvenance::default()
        },
        ..MemoryContinuity::default()
    }
}

fn turn_excerpt(candidate: &RefinedCandidate) -> Option<String> {
    let raw = candidate
        .summary
        .as_deref()
        .or_else(|| candidate.value.as_str())?;
    let excerpt: String = raw.chars().take(240).collect();
    (!excerpt.trim().is_empty()).then_some(excerpt)
}

fn candidate_is_sensitive(candidate: &RefinedCandidate) -> bool {
    let haystack = format!("{} {}", candidate.key, candidate.value).to_ascii_lowercase();
    [
        "password",
        "passcode",
        "api_key",
        "api key",
        "secret",
        "private_key",
        "private key",
        "access_token",
        "access token",
        "credit card",
        "social security",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}
