//! Post-session memory consolidation: forgetting, compression, and evolution.
//!
//! Runs three phases over an agent's curated memories:
//! 1. **Forget** — deterministic pruning of low-value facts and underperforming procedures.
//! 2. **Compress** — LLM-assisted episodic event compression.
//! 3. **Evolve** — LLM-assisted fact merging, contradiction resolution, and insight creation.

use crate::error::MemoryError;
use crate::store::MemoryStoreApi;
use crate::types::{AgentEvent, Fact, FactSource};
use aura_agent::KernelModelGateway;
use aura_core::{AgentEventId, AgentId, FactId};
use aura_reasoner::{Message, ModelProvider, ModelRequest};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::sync::Arc;
use tracing::{debug, info};

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the memory consolidation process.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    /// Model identifier for LLM-assisted consolidation steps.
    pub model: String,
    /// Optional auth token for the model provider proxy.
    pub auth_token: Option<String>,
    /// Trigger event compression when event count exceeds this threshold.
    pub max_events_before_compression: usize,
    /// Maximum age (days) for events before they become compression candidates.
    pub max_event_age_days: i64,
    /// Facts with importance below this and zero access are forgetting candidates.
    pub importance_forget_threshold: f32,
    /// Facts not accessed within this many days are forgetting candidates.
    pub access_forget_days: i64,
    /// Procedures with success rate below this are forgetting candidates.
    pub procedure_forget_threshold: f32,
    /// Minimum execution count before a procedure can be forgotten for low success.
    pub procedure_min_executions: u32,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            auth_token: None,
            max_events_before_compression: 100,
            max_event_age_days: 30,
            importance_forget_threshold: 0.1,
            access_forget_days: 60,
            procedure_forget_threshold: 0.2,
            procedure_min_executions: 3,
        }
    }
}

// ============================================================================
// Report
// ============================================================================

/// Summary of changes applied during a consolidation run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationReport {
    /// Facts merged into richer entries.
    pub facts_merged: usize,
    /// Facts updated with new evidence.
    pub facts_evolved: usize,
    /// Consolidated summary events created.
    pub events_compressed: usize,
    /// Original events deleted during compression.
    pub events_deleted: usize,
    /// Low-value facts pruned.
    pub facts_forgotten: usize,
    /// Underperforming procedures pruned.
    pub procedures_forgotten: usize,
    /// New cross-session insight facts created.
    pub insights_created: usize,
}

// ============================================================================
// Consolidator
// ============================================================================

/// Post-session consolidator that prunes, compresses, and evolves agent memories.
///
/// LLM calls for compression and fact evolution route through a
/// [`KernelModelGateway`] so they are recorded in the kernel's append-only
/// log (Invariant §3).
pub struct MemoryConsolidator {
    store: Arc<dyn MemoryStoreApi>,
    provider: Arc<KernelModelGateway>,
    config: ConsolidationConfig,
}

impl MemoryConsolidator {
    /// Create a new consolidator backed by the given store and kernel gateway.
    #[must_use]
    pub fn new(
        store: Arc<dyn MemoryStoreApi>,
        provider: Arc<KernelModelGateway>,
        config: ConsolidationConfig,
    ) -> Self {
        Self {
            store,
            provider,
            config,
        }
    }

    /// Run the full consolidation pipeline for an agent.
    ///
    /// Executes forget → compress → evolve in sequence and returns a report
    /// summarising all changes.
    ///
    /// # Errors
    /// Returns error on store I/O or model provider failure.
    pub async fn consolidate(&self, agent_id: AgentId) -> Result<ConsolidationReport, MemoryError> {
        let mut report = ConsolidationReport::default();

        self.forget(agent_id, &mut report).await?;
        self.compress_events(agent_id, &mut report).await?;
        self.evolve_facts(agent_id, &mut report).await?;

        info!(
            %agent_id,
            facts_forgotten = report.facts_forgotten,
            procedures_forgotten = report.procedures_forgotten,
            events_compressed = report.events_compressed,
            events_deleted = report.events_deleted,
            facts_merged = report.facts_merged,
            facts_evolved = report.facts_evolved,
            insights_created = report.insights_created,
            "Consolidation complete"
        );

        Ok(report)
    }

    // ========================================================================
    // Phase 1 — Forget (deterministic, no LLM)
    // ========================================================================

    /// Deterministic pruning of low-value facts and underperforming procedures.
    async fn forget(
        &self,
        agent_id: AgentId,
        report: &mut ConsolidationReport,
    ) -> Result<(), MemoryError> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        let (facts_forgotten, procedures_forgotten) = tokio::task::spawn_blocking(move || {
            let mut ff = 0usize;
            let mut pf = 0usize;
            let now = Utc::now();
            let access_cutoff_secs = config.access_forget_days * 86_400;

            let facts = store.list_facts(agent_id)?;
            for fact in &facts {
                let age_secs = (now - fact.updated_at).num_seconds();
                if fact.importance < config.importance_forget_threshold
                    && fact.access_count == 0
                    && age_secs > access_cutoff_secs
                {
                    store.delete_fact(agent_id, fact.fact_id)?;
                    ff += 1;
                }
            }

            let procedures = store.list_procedures(agent_id)?;
            for proc in &procedures {
                if proc.success_rate < config.procedure_forget_threshold
                    && proc.execution_count >= config.procedure_min_executions
                {
                    store.delete_procedure(agent_id, proc.procedure_id)?;
                    pf += 1;
                }
            }
            Ok::<_, MemoryError>((ff, pf))
        })
        .await
        .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??;

        report.facts_forgotten = facts_forgotten;
        report.procedures_forgotten = procedures_forgotten;
        debug!(%agent_id, facts_forgotten, procedures_forgotten, "Forget phase complete");
        Ok(())
    }

    // ========================================================================
    // Phase 2 — Compress events (LLM-assisted)
    // ========================================================================

    /// LLM-assisted compression of oldest episodic events into summaries.
    async fn compress_events(
        &self,
        agent_id: AgentId,
        report: &mut ConsolidationReport,
    ) -> Result<(), MemoryError> {
        // Read phase — blocking
        let store = Arc::clone(&self.store);
        let threshold = self.config.max_events_before_compression;
        let all_events = {
            let s = Arc::clone(&store);
            tokio::task::spawn_blocking(move || s.list_events(agent_id, 100_000))
                .await
                .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??
        };

        if all_events.len() <= threshold {
            return Ok(());
        }

        let overflow = all_events.len() - threshold;
        let oldest: Vec<AgentEvent> = all_events.into_iter().rev().take(overflow).collect();
        if oldest.is_empty() {
            return Ok(());
        }

        // LLM phase — async
        let prompt = Self::build_compress_prompt(&oldest.iter().collect::<Vec<_>>());
        let request = ModelRequest::builder(&self.config.model, COMPRESS_SYSTEM_PROMPT)
            .messages(vec![Message::user(prompt)])
            .max_tokens(2048)
            .auth_token(self.config.auth_token.clone())
            .try_build()
            .map_err(|e| MemoryError::Provider(e.to_string()))?;

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(|e| MemoryError::Provider(e.to_string()))?;

        let text = response.message.text_content();
        let summaries = Self::parse_compress_response(&text);

        if summaries.is_empty() {
            return Ok(());
        }

        // Write phase — blocking
        let s = Arc::clone(&store);
        let (compressed, deleted) = tokio::task::spawn_blocking(move || {
            let now = Utc::now();
            for summary in &summaries {
                let event = AgentEvent {
                    event_id: AgentEventId::generate(),
                    agent_id,
                    event_type: "consolidated_summary".to_string(),
                    summary: summary.clone(),
                    metadata: serde_json::json!({
                        "source": "consolidation",
                        "original_count": overflow
                    }),
                    importance: 0.6,
                    access_count: 0,
                    last_accessed: now,
                    timestamp: now,
                };
                s.put_event(&event)?;
            }
            for event in &oldest {
                s.delete_event_direct(agent_id, event.timestamp, event.event_id)?;
            }
            Ok::<_, MemoryError>((summaries.len(), oldest.len()))
        })
        .await
        .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??;

        report.events_compressed = compressed;
        report.events_deleted = deleted;
        debug!(%agent_id, compressed, deleted, "Compress phase complete");
        Ok(())
    }

    fn build_compress_prompt(events: &[&AgentEvent]) -> String {
        let mut prompt = String::from(
            "Summarize the following episodic events into a few higher-level summaries.\n\
             Each summary should capture the key outcome or pattern from a group of related events.\n\
             Respond with one summary per line, prefixed with \"SUMMARY: \".\n\nEvents:\n",
        );

        for (i, event) in events.iter().enumerate() {
            let _ = writeln!(
                prompt,
                "{}. [{}] {}: {}",
                i + 1,
                event.timestamp.format("%Y-%m-%d"),
                event.event_type,
                event.summary
            );
        }

        prompt
    }

    fn parse_compress_response(response: &str) -> Vec<String> {
        response
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                trimmed
                    .strip_prefix("SUMMARY: ")
                    .or_else(|| trimmed.strip_prefix("SUMMARY:"))
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .collect()
    }

    // ========================================================================
    // Phase 3 — Evolve facts (LLM-assisted)
    // ========================================================================

    /// LLM-assisted fact analysis: merging, contradiction resolution, insights.
    async fn evolve_facts(
        &self,
        agent_id: AgentId,
        report: &mut ConsolidationReport,
    ) -> Result<(), MemoryError> {
        // Read phase — blocking
        let store = Arc::clone(&self.store);
        let (facts, recent_events) = {
            let s = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                let facts = s.list_facts(agent_id)?;
                let events = s.list_events(agent_id, 50)?;
                Ok::<_, MemoryError>((facts, events))
            })
            .await
            .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??
        };

        if facts.len() < 2 {
            return Ok(());
        }

        // LLM phase — async
        let prompt = Self::build_evolve_prompt(&facts, &recent_events);
        let request = ModelRequest::builder(&self.config.model, EVOLVE_SYSTEM_PROMPT)
            .messages(vec![Message::user(prompt)])
            .max_tokens(2048)
            .auth_token(self.config.auth_token.clone())
            .try_build()
            .map_err(|e| MemoryError::Provider(e.to_string()))?;

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(|e| MemoryError::Provider(e.to_string()))?;

        let text = response.message.text_content();

        // Write phase — blocking (apply_evolution logic inlined)
        let s = Arc::clone(&store);
        let (merged, evolved, insights) =
            tokio::task::spawn_blocking(move || apply_evolution(&*s, &text, agent_id, &facts))
                .await
                .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))??;

        report.facts_merged = merged;
        report.facts_evolved = evolved;
        report.insights_created = insights;

        debug!(
            %agent_id,
            merged = report.facts_merged,
            evolved = report.facts_evolved,
            insights = report.insights_created,
            "Evolve phase complete"
        );

        Ok(())
    }

    fn build_evolve_prompt(facts: &[Fact], recent_events: &[AgentEvent]) -> String {
        let mut prompt = String::from("Current facts:\n");

        for (i, fact) in facts.iter().enumerate() {
            let val = match &fact.value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let _ = writeln!(
                prompt,
                "{}. key=\"{}\" value=\"{}\" confidence={:.2} importance={:.2}",
                i + 1,
                fact.key,
                val,
                fact.confidence,
                fact.importance
            );
        }

        if !recent_events.is_empty() {
            prompt.push_str("\nRecent events:\n");
            for event in recent_events.iter().take(20) {
                let _ = writeln!(
                    prompt,
                    "- [{}] {}: {}",
                    event.timestamp.format("%Y-%m-%d"),
                    event.event_type,
                    event.summary
                );
            }
        }

        prompt.push_str(
            "\nAnalyze these facts and respond with actions, one per line:\n\
             - MERGE <N> <M> key=\"merged_key\" value=\"merged_value\" — merge fact N into M\n\
             - EVOLVE <N> value=\"new_value\" confidence=X.X — update a fact with new evidence\n\
             - INSIGHT key=\"key\" value=\"value\" — create a new cross-session insight\n\
             Only suggest changes that are clearly beneficial. It is fine to suggest nothing.",
        );

        prompt
    }
}

/// Apply LLM-recommended evolution actions to the fact store.
/// Returns (facts_merged, facts_evolved, insights_created).
fn apply_evolution(
    store: &dyn MemoryStoreApi,
    response: &str,
    agent_id: AgentId,
    facts: &[Fact],
) -> Result<(usize, usize, usize), MemoryError> {
    let now = Utc::now();
    let mut to_delete = Vec::new();
    let mut evolved_indices = Vec::new();
    let mut merged = 0usize;
    let mut evolved = 0usize;
    let mut insights = 0usize;

    for line in response.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("MERGE ") {
            if let Some((src, dst)) = parse_merge_indices(rest, facts.len()) {
                if to_delete.contains(&src) || to_delete.contains(&dst) {
                    continue;
                }
                to_delete.push(src);
                let mut target = facts[dst].clone();
                if let Some(val) = extract_quoted_value(rest, "value=") {
                    target.value = serde_json::Value::String(val);
                }
                if let Some(key) = extract_quoted_value(rest, "key=") {
                    target.key = key;
                }
                target.source = FactSource::Consolidated;
                target.updated_at = now;
                store.put_fact(&target)?;
                merged += 1;
            }
        } else if let Some(rest) = trimmed.strip_prefix("EVOLVE ") {
            if let Some(idx) = parse_single_index(rest, facts.len()) {
                if evolved_indices.contains(&idx) || to_delete.contains(&idx) {
                    continue;
                }
                evolved_indices.push(idx);
                let mut fact = facts[idx].clone();
                if let Some(val) = extract_quoted_value(rest, "value=") {
                    fact.value = serde_json::Value::String(val);
                }
                if let Some(conf) = extract_float_value(rest, "confidence=") {
                    fact.confidence = conf;
                }
                fact.source = FactSource::Consolidated;
                fact.updated_at = now;
                store.put_fact(&fact)?;
                evolved += 1;
            }
        } else if let Some(rest) = trimmed.strip_prefix("INSIGHT ") {
            let key = extract_quoted_value(rest, "key=");
            let value = extract_quoted_value(rest, "value=");
            if let (Some(k), Some(v)) = (key, value) {
                let fact = Fact {
                    fact_id: FactId::generate(),
                    agent_id,
                    key: k,
                    value: serde_json::Value::String(v),
                    confidence: 0.7,
                    source: FactSource::Consolidated,
                    importance: 0.5,
                    access_count: 0,
                    last_accessed: now,
                    created_at: now,
                    updated_at: now,
                };
                store.put_fact(&fact)?;
                insights += 1;
            }
        }
    }

    for &idx in &to_delete {
        store.delete_fact(agent_id, facts[idx].fact_id)?;
    }

    Ok((merged, evolved, insights))
}

// ============================================================================
// Parsing helpers
// ============================================================================

/// Parse two 1-based indices from "N M ..." into 0-based, validating bounds.
fn parse_merge_indices(text: &str, max: usize) -> Option<(usize, usize)> {
    let mut parts = text.split_whitespace();
    let src = parts.next()?.parse::<usize>().ok()?.checked_sub(1)?;
    let dst = parts.next()?.parse::<usize>().ok()?.checked_sub(1)?;
    if src < max && dst < max && src != dst {
        Some((src, dst))
    } else {
        None
    }
}

/// Parse a single 1-based index from "N ..." into 0-based, validating bounds.
fn parse_single_index(text: &str, max: usize) -> Option<usize> {
    let idx = text
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    if idx < max {
        Some(idx)
    } else {
        None
    }
}

/// Extract a quoted or bare value after the given `prefix=` marker.
fn extract_quoted_value(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)? + prefix.len();
    let rest = &text[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Extract a float value after the given `prefix=` marker.
fn extract_float_value(text: &str, prefix: &str) -> Option<f32> {
    let start = text.find(prefix)? + prefix.len();
    let end = text[start..]
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or(text.len(), |e| start + e);
    text[start..end].parse().ok()
}

// ============================================================================
// System prompts
// ============================================================================

const COMPRESS_SYSTEM_PROMPT: &str = "\
You are a memory consolidator for an AI agent. Given a list of episodic events \
from past sessions, compress them into a smaller set of higher-level summaries \
that preserve key outcomes, patterns, and lessons learned. Respond with one \
summary per line, each prefixed with \"SUMMARY: \". Focus on actionable \
knowledge and significant outcomes.";

const EVOLVE_SYSTEM_PROMPT: &str = "\
You are a memory analyst for an AI agent. Given the agent's current facts and \
recent events, identify: (1) facts that should be merged because they describe \
the same thing, (2) facts that should be updated based on new evidence, and \
(3) new insights derivable from cross-session patterns. Respond with one action \
per line using the exact formats specified. Be conservative — only suggest \
changes with clear benefit.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compress_response_with_summaries() {
        let response = "SUMMARY: First summary\nSUMMARY: Second summary\nNot a summary";
        let results = MemoryConsolidator::parse_compress_response(response);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], "First summary");
        assert_eq!(results[1], "Second summary");
    }

    #[test]
    fn parse_compress_response_empty_lines_filtered() {
        let response = "SUMMARY: \nSUMMARY: Valid\n\n";
        let results = MemoryConsolidator::parse_compress_response(response);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "Valid");
    }

    #[test]
    fn parse_compress_response_no_prefix() {
        let response = "Just some text\nAnother line";
        let results = MemoryConsolidator::parse_compress_response(response);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_compress_response_colon_without_space() {
        let response = "SUMMARY:no space after colon";
        let results = MemoryConsolidator::parse_compress_response(response);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn parse_merge_indices_valid() {
        assert_eq!(parse_merge_indices("1 3 key=\"merged\"", 5), Some((0, 2)));
    }

    #[test]
    fn parse_merge_indices_out_of_bounds() {
        assert_eq!(parse_merge_indices("1 10 key=\"m\"", 5), None);
    }

    #[test]
    fn parse_merge_indices_same() {
        assert_eq!(parse_merge_indices("2 2 key=\"m\"", 5), None);
    }

    #[test]
    fn parse_single_index_valid() {
        assert_eq!(parse_single_index("3 value=\"x\"", 5), Some(2));
    }

    #[test]
    fn parse_single_index_out_of_bounds() {
        assert_eq!(parse_single_index("6 value=\"x\"", 5), None);
    }

    #[test]
    fn parse_single_index_zero() {
        assert_eq!(parse_single_index("0 value=\"x\"", 5), None);
    }

    #[test]
    fn extract_quoted_value_double_quoted() {
        let result = extract_quoted_value("key=\"hello world\"", "key=");
        assert_eq!(result.unwrap(), "hello world");
    }

    #[test]
    fn extract_quoted_value_bare() {
        let result = extract_quoted_value("value=bare rest", "value=");
        assert_eq!(result.unwrap(), "bare");
    }

    #[test]
    fn extract_quoted_value_missing() {
        assert!(extract_quoted_value("no match here", "key=").is_none());
    }

    #[test]
    fn extract_float_value_valid() {
        let result = extract_float_value("confidence=0.85 rest", "confidence=");
        assert!((result.unwrap() - 0.85).abs() < 1e-3);
    }

    #[test]
    fn extract_float_value_missing() {
        assert!(extract_float_value("no match", "confidence=").is_none());
    }
}
