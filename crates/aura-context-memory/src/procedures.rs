//! Procedural memory extraction, matching, and feedback.
//!
//! Detects repeated tool-call patterns across agent sessions, promotes them
//! to named [`Procedure`]s, and matches stored procedures to new tasks via
//! keyword overlap with their trigger text.

use crate::error::MemoryError;
use crate::store::MemoryStoreApi;
use crate::types::Procedure;
use aura_core::{AgentId, ProcedureId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

/// Configuration for procedural memory extraction.
#[derive(Debug, Clone)]
pub struct ProcedureConfig {
    /// Minimum occurrences of a pattern before it becomes a procedure.
    pub min_occurrences: usize,
    /// Minimum number of steps in a sequence to be considered.
    pub min_steps: usize,
    /// Maximum number of procedures stored per agent.
    pub max_procedures: usize,
}

impl Default for ProcedureConfig {
    fn default() -> Self {
        Self {
            min_occurrences: 2,
            min_steps: 3,
            max_procedures: 50,
        }
    }
}

/// Extracts, matches, and updates procedural memory for agents.
///
/// Tracks tool-call step patterns across sessions. When a pattern recurs
/// at least `min_occurrences` times it is promoted to a stored [`Procedure`].
pub struct ProcedureExtractor {
    store: Arc<dyn MemoryStoreApi>,
    config: ProcedureConfig,
}

/// A sequence of tool calls observed during a single agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepSequence {
    /// Ordered tool names executed during the turn.
    pub steps: Vec<String>,
    /// Optional user message or task description associated with the sequence.
    pub task_hint: Option<String>,
    /// Whether the overall turn was considered successful.
    pub succeeded: bool,
    /// Skill that was active when this sequence was observed.
    #[serde(default)]
    pub skill_name: Option<String>,
    /// Description of the active skill (used for relevance scoring).
    #[serde(default)]
    pub skill_description: Option<String>,
}

/// Minimum similarity ratio (0.0–1.0) for two step sequences to be
/// considered the same pattern.
const SIMILARITY_THRESHOLD: f32 = 0.7;

impl ProcedureExtractor {
    /// Create a new extractor backed by the given store and configuration.
    #[must_use]
    pub fn new(store: Arc<dyn MemoryStoreApi>, config: ProcedureConfig) -> Self {
        Self { store, config }
    }

    /// Extract procedural patterns from a sequence of tool-call names.
    ///
    /// Compares the sequence against existing procedures and recent events.
    /// When a pattern has been observed at least `min_occurrences` times it
    /// is promoted to a stored [`Procedure`].
    ///
    /// # Errors
    ///
    /// Returns an error on store read/write failure.
    pub fn extract_from_steps(
        &self,
        agent_id: AgentId,
        sequence: &StepSequence,
    ) -> Result<Option<Procedure>, MemoryError> {
        if sequence.steps.len() < self.config.min_steps {
            return Ok(None);
        }

        if let Some(updated) = self.try_update_existing(agent_id, sequence)? {
            return Ok(Some(updated));
        }

        self.try_promote_pattern(agent_id, sequence)
    }

    /// Match stored procedures to a task by keyword overlap with triggers.
    ///
    /// Returns procedures sorted by descending relevance score.
    ///
    /// # Errors
    ///
    /// Returns an error on store read failure.
    #[allow(clippy::cast_precision_loss)]
    pub fn match_procedures(
        &self,
        agent_id: AgentId,
        task_text: &str,
    ) -> Result<Vec<Procedure>, MemoryError> {
        let procedures = self.store.list_procedures(agent_id)?;
        let task_words: Vec<&str> = tokenize_words(task_text);

        let mut scored: Vec<(Procedure, f32)> = procedures
            .into_iter()
            .filter_map(|proc| {
                let trigger_words: Vec<&str> = tokenize_words(&proc.trigger);
                let overlap = task_words
                    .iter()
                    .filter(|tw| trigger_words.iter().any(|pw| pw.eq_ignore_ascii_case(tw)))
                    .count();

                if overlap > 0 {
                    let score = overlap as f32 / trigger_words.len().max(1) as f32;
                    Some((proc, score))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().map(|(p, _)| p).collect())
    }

    /// Record feedback for a procedure after execution.
    ///
    /// Updates the success rate via exponential moving average and optionally
    /// refines the stored steps if the actual execution diverged.
    ///
    /// # Errors
    ///
    /// Returns an error on store read/write failure or if the procedure is
    /// not found.
    pub fn record_feedback(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
        succeeded: bool,
        actual_steps: Option<&[String]>,
    ) -> Result<(), MemoryError> {
        let mut proc = self.store.get_procedure(agent_id, procedure_id)?;
        proc.execution_count += 1;
        proc.last_used = Utc::now();
        proc.updated_at = Utc::now();

        apply_success_ema(&mut proc.success_rate, succeeded);

        if let Some(actual) = actual_steps {
            if actual != proc.steps.as_slice() {
                proc.steps = merge_steps(&proc.steps, actual);
            }
        }

        self.store.put_procedure(&proc)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    /// Try to match `sequence` against an existing procedure and update it.
    fn try_update_existing(
        &self,
        agent_id: AgentId,
        sequence: &StepSequence,
    ) -> Result<Option<Procedure>, MemoryError> {
        let existing = self.store.list_procedures(agent_id)?;
        for mut proc in existing {
            if sequences_match(&proc.steps, &sequence.steps) {
                proc.execution_count += 1;
                proc.last_used = Utc::now();
                proc.updated_at = Utc::now();
                apply_success_ema(&mut proc.success_rate, sequence.succeeded);
                if proc.steps != sequence.steps {
                    proc.steps = merge_steps(&proc.steps, &sequence.steps);
                }
                self.store.put_procedure(&proc)?;
                debug!(name = %proc.name, executions = proc.execution_count, "Updated procedure");
                return Ok(Some(proc));
            }
        }
        Ok(None)
    }

    /// Check recent events for a recurring pattern and promote to a procedure.
    fn try_promote_pattern(
        &self,
        agent_id: AgentId,
        sequence: &StepSequence,
    ) -> Result<Option<Procedure>, MemoryError> {
        let events = self.store.list_events(agent_id, 50)?;
        let similar_count = events
            .iter()
            .filter(|e| {
                e.metadata
                    .get("tool_sequence")
                    .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
                    .is_some_and(|prev| sequences_match(&prev, &sequence.steps))
            })
            .count();

        // +1 for the current sequence itself.
        if similar_count + 1 < self.config.min_occurrences {
            return Ok(None);
        }

        let now = Utc::now();
        let name = derive_name(&sequence.steps, sequence.task_hint.as_deref());
        let trigger = sequence
            .task_hint
            .clone()
            .unwrap_or_else(|| sequence.steps.first().cloned().unwrap_or_default());

        let skill_relevance = sequence.skill_name.as_ref().map(|_| {
            compute_skill_relevance(
                &name,
                &trigger,
                &sequence.steps,
                sequence.skill_description.as_deref().unwrap_or(""),
            )
        });

        let procedure = Procedure {
            procedure_id: ProcedureId::generate(),
            agent_id,
            name,
            trigger,
            steps: sequence.steps.clone(),
            context_constraints: serde_json::Value::Null,
            success_rate: if sequence.succeeded { 1.0 } else { 0.0 },
            execution_count: 1,
            last_used: now,
            created_at: now,
            updated_at: now,
            skill_name: sequence.skill_name.clone(),
            skill_relevance,
        };

        self.store.put_procedure(&procedure)?;
        self.enforce_capacity(agent_id)?;
        info!(name = %procedure.name, steps = procedure.steps.len(), "Created new procedure");
        Ok(Some(procedure))
    }

    /// Evict lowest-value procedures when capacity is exceeded.
    #[allow(clippy::cast_precision_loss)]
    fn enforce_capacity(&self, agent_id: AgentId) -> Result<(), MemoryError> {
        let mut procs = self.store.list_procedures(agent_id)?;
        if procs.len() <= self.config.max_procedures {
            return Ok(());
        }

        procs.sort_by(|a, b| {
            let score_a = a.success_rate * a.execution_count as f32;
            let score_b = b.success_rate * b.execution_count as f32;
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let to_remove = procs.len() - self.config.max_procedures;
        for proc in procs.iter().take(to_remove) {
            self.store.delete_procedure(agent_id, proc.procedure_id)?;
        }
        Ok(())
    }
}

// ======================================================================
// Free-standing helpers (avoid `unused_self` pedantic lint)
// ======================================================================

/// Check whether two step sequences are similar enough to be the same
/// pattern.  Tolerates up to one step difference in length and requires
/// at least [`SIMILARITY_THRESHOLD`] overlap.
#[allow(clippy::cast_precision_loss)]
fn sequences_match(a: &[String], b: &[String]) -> bool {
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let min_len = a.len().min(b.len());
    if min_len == 0 {
        return false;
    }
    let matching = a.iter().zip(b.iter()).filter(|(x, y)| x == y).count();
    let similarity = matching as f32 / min_len as f32;
    similarity >= SIMILARITY_THRESHOLD
}

/// Merge two step sequences, preferring the newer (or longer) one.
fn merge_steps(existing: &[String], new: &[String]) -> Vec<String> {
    if new.len() >= existing.len() {
        new.to_vec()
    } else {
        existing.to_vec()
    }
}

/// Derive a human-readable name for a procedure from its steps or task hint.
fn derive_name(steps: &[String], task_hint: Option<&str>) -> String {
    if let Some(hint) = task_hint {
        let words: Vec<&str> = hint.split_whitespace().take(5).collect();
        if !words.is_empty() {
            return words.join(" ").to_lowercase();
        }
    }
    let key_steps: Vec<&str> = steps
        .iter()
        .filter(|s| !s.contains("read"))
        .take(3)
        .map(String::as_str)
        .collect();
    if key_steps.is_empty() {
        "unnamed_procedure".to_string()
    } else {
        key_steps.join(" -> ")
    }
}

/// Apply an exponential moving average update to a success rate.
fn apply_success_ema(rate: &mut f32, succeeded: bool) {
    if succeeded {
        *rate = 0.8f32.mul_add(*rate, 0.2);
    } else {
        *rate *= 0.8;
    }
}

/// Split text into lowercase-trimmed words suitable for keyword matching.
fn tokenize_words(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| w.len() > 2)
        .collect()
}

/// Compute how relevant a procedure is to a skill based on word overlap
/// between the procedure's content (name, trigger, steps) and the skill
/// description. Returns a score in 0.0–1.0.
#[allow(clippy::cast_precision_loss)]
pub fn compute_skill_relevance(
    proc_name: &str,
    proc_trigger: &str,
    proc_steps: &[String],
    skill_description: &str,
) -> f32 {
    if skill_description.is_empty() {
        return 0.5;
    }

    let skill_words: Vec<&str> = tokenize_words(skill_description);
    if skill_words.is_empty() {
        return 0.5;
    }

    let steps_text = proc_steps.join(" ");
    let proc_text = format!("{proc_name} {proc_trigger} {steps_text}");
    let proc_words: Vec<&str> = tokenize_words(&proc_text);
    if proc_words.is_empty() {
        return 0.0;
    }

    let overlap = proc_words
        .iter()
        .filter(|pw| skill_words.iter().any(|sw| sw.eq_ignore_ascii_case(pw)))
        .count();

    let score = overlap as f32 / proc_words.len().max(1) as f32;
    score.min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_match_length_diff_gt_1() {
        let a: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let b: Vec<String> = vec!["a".into()];
        assert!(!sequences_match(&a, &b));
    }

    #[test]
    fn sequences_match_above_threshold() {
        let a: Vec<String> = vec!["read".into(), "write".into(), "build".into(), "test".into()];
        let b: Vec<String> = vec![
            "read".into(),
            "write".into(),
            "build".into(),
            "deploy".into(),
        ];
        assert!(sequences_match(&a, &b));
    }

    #[test]
    fn sequences_match_below_threshold() {
        let a: Vec<String> = vec!["read".into(), "write".into(), "build".into()];
        let b: Vec<String> = vec!["deploy".into(), "test".into(), "lint".into()];
        assert!(!sequences_match(&a, &b));
    }

    #[test]
    fn sequences_match_empty() {
        let a: Vec<String> = vec![];
        let b: Vec<String> = vec![];
        assert!(!sequences_match(&a, &b));
    }

    #[test]
    fn merge_steps_longer_new_preferred() {
        let existing = vec!["a".into(), "b".into()];
        let new = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(merge_steps(&existing, &new), new);
    }

    #[test]
    fn merge_steps_shorter_existing_kept() {
        let existing = vec!["a".into(), "b".into(), "c".into()];
        let new = vec!["a".into(), "b".into()];
        assert_eq!(merge_steps(&existing, &new), existing);
    }

    #[test]
    fn derive_name_from_hint() {
        let steps = vec!["a".into()];
        let name = derive_name(&steps, Some("deploy the application now"));
        assert_eq!(name, "deploy the application now");
    }

    #[test]
    fn derive_name_from_steps() {
        let steps = vec!["build".into(), "test".into(), "deploy".into()];
        let name = derive_name(&steps, None);
        assert_eq!(name, "build -> test -> deploy");
    }

    #[test]
    fn derive_name_filters_read() {
        let steps = vec!["read".into(), "build".into(), "deploy".into()];
        let name = derive_name(&steps, None);
        assert_eq!(name, "build -> deploy");
    }

    #[test]
    fn derive_name_all_read_unnamed() {
        let steps = vec!["read_file".into(), "read_dir".into()];
        let name = derive_name(&steps, None);
        assert_eq!(name, "unnamed_procedure");
    }

    #[test]
    fn apply_success_ema_success() {
        let mut rate = 0.5;
        apply_success_ema(&mut rate, true);
        let expected = 0.8f32.mul_add(0.5, 0.2);
        assert!((rate - expected).abs() < 1e-6);
    }

    #[test]
    fn apply_success_ema_failure() {
        let mut rate = 0.5;
        apply_success_ema(&mut rate, false);
        assert!((rate - 0.4).abs() < 1e-6);
    }

    #[test]
    fn tokenize_words_basic() {
        let words = tokenize_words("hello, world! foo ba");
        assert!(words.contains(&"hello"));
        assert!(words.contains(&"world"));
        assert!(words.contains(&"foo"));
        assert!(!words.contains(&"ba"));
    }

    #[test]
    fn tokenize_words_punctuation_stripped() {
        let words = tokenize_words("(deploy) [build]");
        assert!(words.contains(&"deploy"));
        assert!(words.contains(&"build"));
    }

    #[test]
    fn tokenize_words_min_length_3() {
        let words = tokenize_words("a ab abc abcd");
        assert!(!words.contains(&"a"));
        assert!(!words.contains(&"ab"));
        assert!(words.contains(&"abc"));
        assert!(words.contains(&"abcd"));
    }

    #[test]
    fn skill_relevance_high_overlap() {
        let score = compute_skill_relevance(
            "deploy production",
            "deploy the application",
            &["build".into(), "push".into(), "deploy".into()],
            "deploy applications to production environments",
        );
        assert!(score > 0.3, "expected high relevance, got {score}");
    }

    #[test]
    fn skill_relevance_no_overlap() {
        let score = compute_skill_relevance(
            "user preferences",
            "remember dark mode setting",
            &["store_preference".into()],
            "deploy applications to production environments",
        );
        assert!(score < 0.3, "expected low relevance, got {score}");
    }

    #[test]
    fn skill_relevance_empty_description() {
        let score = compute_skill_relevance("deploy flow", "deploy", &["build".into()], "");
        assert!((score - 0.5).abs() < f32::EPSILON);
    }
}
