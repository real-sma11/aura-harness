//! Query-aware retrieval of memory for system-prompt injection.

use crate::error::MemoryError;
use crate::salience;
use crate::store::MemoryStoreApi;
use crate::types::{
    AgentEvent, Fact, MemoryPacket, MemoryRetrievalMode, MemoryRetrievalTrace, MemoryScope,
    MemorySelection, MemorySensitivity, MemoryStatus, Procedure,
};
use aura_core_types::AgentId;
use chrono::Utc;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

/// Current-turn signal used to choose relevant memories.
#[derive(Debug, Clone, Default)]
pub struct MemoryQueryContext {
    pub text: String,
    pub active_skills: Vec<String>,
    pub allow_user_scope: bool,
    pub allow_workspace_scope: bool,
}

/// Retrieves and ranks agent memory for system-prompt injection.
pub struct MemoryRetriever {
    store: Arc<dyn MemoryStoreApi>,
    config: RetrievalConfig,
}

/// Configuration for memory retrieval, scoring, and budget enforcement.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    pub max_facts: usize,
    pub max_events: usize,
    pub max_procedures: usize,
    pub min_confidence: f32,
    /// Maximum estimated tokens for the memory injection.
    pub token_budget: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            max_facts: 12,
            max_events: 6,
            max_procedures: 4,
            min_confidence: 0.3,
            token_budget: 800,
        }
    }
}

impl MemoryRetriever {
    #[must_use]
    pub fn new(store: Arc<dyn MemoryStoreApi>, config: RetrievalConfig) -> Self {
        Self { store, config }
    }

    /// Backward-compatible salience retrieval without a turn query.
    pub async fn retrieve(&self, agent_id: AgentId) -> Result<MemoryPacket, MemoryError> {
        self.retrieve_with_query(
            agent_id,
            MemoryQueryContext::default(),
            MemoryRetrievalMode::Salience,
        )
        .await
    }

    /// Retrieve a budgeted packet using current-turn relevance plus durable
    /// salience. The trace contains IDs and scores but never the query text.
    pub async fn retrieve_with_query(
        &self,
        agent_id: AgentId,
        query: MemoryQueryContext,
        mode: MemoryRetrievalMode,
    ) -> Result<MemoryPacket, MemoryError> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let now = Utc::now();
            let query_tokens = tokenize(&query.text);
            let query_aware = mode == MemoryRetrievalMode::QueryAware && !query_tokens.is_empty();
            let active_skills: HashSet<String> = query
                .active_skills
                .iter()
                .map(|skill| skill.to_ascii_lowercase())
                .collect();

            let mut facts = store.list_facts(agent_id)?;
            let mut events =
                store.list_events(agent_id, config.max_events.saturating_mul(10).max(50))?;
            let mut procedures = store.list_procedures(agent_id)?;
            let candidate_count = facts.len() + events.len() + procedures.len();

            facts.retain(|fact| {
                fact.confidence >= config.min_confidence && eligible(&fact.continuity, &query)
            });
            events.retain(|event| eligible(&event.continuity, &query));
            procedures.retain(|procedure| eligible(&procedure.continuity, &query));

            let mut scored_facts: Vec<_> = facts
                .into_iter()
                .filter_map(|fact| {
                    let relevance = if query_aware {
                        lexical_relevance(&query_tokens, &fact_text(&fact))
                    } else {
                        0.0
                    };
                    if query_aware && relevance == 0.0 && !fact.continuity.pinned {
                        return None;
                    }
                    let base = salience::score_fact(&fact, now);
                    let pinned = if fact.continuity.pinned { 0.75 } else { 0.0 };
                    let score = if query_aware {
                        relevance.mul_add(0.65, base * 0.35) + pinned
                    } else {
                        base + pinned
                    };
                    Some((fact, score, relevance))
                })
                .collect();
            scored_facts.sort_by(score_order);
            scored_facts.truncate(config.max_facts);

            let mut scored_events: Vec<_> = events
                .into_iter()
                .filter_map(|event| {
                    let relevance = if query_aware {
                        lexical_relevance(&query_tokens, &event_text(&event))
                    } else {
                        0.0
                    };
                    if query_aware && relevance == 0.0 && event.importance < 0.85 {
                        return None;
                    }
                    let base = salience::score_event(&event, now);
                    let score = if query_aware {
                        relevance.mul_add(0.7, base * 0.3)
                    } else {
                        base
                    };
                    Some((event, score, relevance))
                })
                .collect();
            scored_events.sort_by(score_order);
            scored_events.truncate(config.max_events);

            let mut scored_procedures: Vec<_> =
                procedures
                    .into_iter()
                    .filter_map(|procedure| {
                        let mut relevance = if query_aware {
                            lexical_relevance(&query_tokens, &procedure_text(&procedure))
                        } else {
                            0.0
                        };
                        if procedure.skill_name.as_ref().is_some_and(|skill| {
                            active_skills.contains(&skill.to_ascii_lowercase())
                        }) {
                            relevance = (relevance + 0.5).min(1.0);
                        }
                        if query_aware && relevance == 0.0 && !procedure.continuity.pinned {
                            return None;
                        }
                        let base = salience::score_procedure(&procedure, now);
                        let pinned = if procedure.continuity.pinned {
                            0.75
                        } else {
                            0.0
                        };
                        let score = if query_aware {
                            relevance.mul_add(0.7, base * 0.3) + pinned
                        } else {
                            base + pinned
                        };
                        Some((procedure, score, relevance))
                    })
                    .collect();
            scored_procedures.sort_by(score_order);
            scored_procedures.truncate(config.max_procedures);

            let mut budget = config.token_budget;
            let mut selections = Vec::new();
            let facts = select_budgeted(
                scored_facts,
                &mut budget,
                salience::estimate_fact_tokens,
                "fact",
                |fact| fact.fact_id.to_hex(),
                &mut selections,
            );
            let events = select_budgeted(
                scored_events,
                &mut budget,
                salience::estimate_event_tokens,
                "event",
                |event| event.event_id.to_hex(),
                &mut selections,
            );
            let procedures = select_budgeted(
                scored_procedures,
                &mut budget,
                salience::estimate_procedure_tokens,
                "procedure",
                |procedure| procedure.procedure_id.to_hex(),
                &mut selections,
            );

            for fact in &facts {
                store.touch_fact(fact.agent_id, fact.fact_id)?;
            }

            let estimated_tokens = config.token_budget.saturating_sub(budget);
            let trace = MemoryRetrievalTrace {
                candidate_count,
                selected_count: selections.len(),
                estimated_tokens,
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                query_aware,
                selections,
            };

            Ok(MemoryPacket {
                facts,
                events,
                procedures,
                trace: Some(trace),
            })
        })
        .await
        .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))?
    }
}

fn eligible(continuity: &crate::types::MemoryContinuity, query: &MemoryQueryContext) -> bool {
    continuity.status == MemoryStatus::Active
        && continuity.sensitivity == MemorySensitivity::Normal
        && match continuity.scope {
            MemoryScope::Agent => true,
            MemoryScope::User => query.allow_user_scope,
            MemoryScope::Workspace => query.allow_workspace_scope,
        }
}

fn score_order<T>(a: &(T, f32, f32), b: &(T, f32, f32)) -> std::cmp::Ordering {
    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
}

fn select_budgeted<T>(
    items: Vec<(T, f32, f32)>,
    budget: &mut usize,
    estimator: fn(&T) -> usize,
    kind: &str,
    id: fn(&T) -> String,
    selections: &mut Vec<MemorySelection>,
) -> Vec<T> {
    let mut result = Vec::new();
    for (item, score, relevance) in items {
        let tokens = estimator(&item);
        if tokens > *budget {
            continue;
        }
        *budget -= tokens;
        selections.push(MemorySelection {
            memory_id: id(&item),
            kind: kind.to_string(),
            score,
            relevance,
            reason: if relevance > 0.0 {
                "current_request".to_string()
            } else {
                "durable_salience".to_string()
            },
        });
        result.push(item);
    }
    result
}

fn fact_text(fact: &Fact) -> String {
    format!("{} {}", fact.key, fact.value)
}

fn event_text(event: &AgentEvent) -> String {
    format!("{} {} {}", event.event_type, event.summary, event.metadata)
}

fn procedure_text(procedure: &Procedure) -> String {
    format!(
        "{} {} {} {}",
        procedure.name,
        procedure.trigger,
        procedure.steps.join(" "),
        procedure.skill_name.as_deref().unwrap_or_default()
    )
}

fn lexical_relevance(query: &HashSet<String>, document: &str) -> f32 {
    if query.is_empty() {
        return 0.0;
    }
    let document = tokenize(document);
    if document.is_empty() {
        return 0.0;
    }
    let overlap = query.intersection(&document).count() as f32;
    overlap / ((query.len() as f32 * document.len() as f32).sqrt().max(1.0))
}

fn tokenize(text: &str) -> HashSet<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "i", "in", "is", "it",
        "my", "of", "on", "or", "that", "the", "this", "to", "we", "what", "when", "with", "you",
    ];
    // Memory keys and procedure names commonly use snake_case/kebab-case,
    // while user requests use spaces. Split all punctuation so those forms
    // share the same semantic tokens.
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 1 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

#[cfg(test)]
mod query_tests {
    use super::*;

    #[test]
    fn lexical_relevance_prefers_matching_meaning_tokens() {
        let query = tokenize("Which test command should I run?");
        let matching = lexical_relevance(&query, "test_command npm test");
        let unrelated = lexical_relevance(&query, "favorite color blue");
        assert!(matching > unrelated);
    }

    #[test]
    fn tokenizer_removes_common_stop_words() {
        let tokens = tokenize("What is the build command for this project?");
        assert!(tokens.contains("build"));
        assert!(tokens.contains("command"));
        assert!(!tokens.contains("the"));
    }
}
