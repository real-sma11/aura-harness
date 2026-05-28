//! Deterministic retrieval of memory for system prompt injection.

use crate::error::MemoryError;
use crate::salience;
use crate::store::MemoryStoreApi;
use crate::types::MemoryPacket;
use aura_core::AgentId;
use chrono::Utc;
use std::sync::Arc;

/// Retrieves and ranks agent memory for system prompt injection.
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
            max_facts: 20,
            max_events: 10,
            max_procedures: 5,
            min_confidence: 0.3,
            token_budget: 2000,
        }
    }
}

impl MemoryRetriever {
    #[must_use]
    pub fn new(store: Arc<dyn MemoryStoreApi>, config: RetrievalConfig) -> Self {
        Self { store, config }
    }

    /// Retrieve a [`MemoryPacket`] for injection into the agent's system prompt.
    ///
    /// Items are scored by salience (importance, recency, access frequency),
    /// sorted by score descending, and trimmed to fit within the configured
    /// token budget. Selected facts have their access tracking updated.
    ///
    /// # Errors
    /// Returns error on store read/write failure.
    pub async fn retrieve(&self, agent_id: AgentId) -> Result<MemoryPacket, MemoryError> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let now = Utc::now();

            let mut facts = store.list_facts(agent_id)?;
            facts.retain(|f| f.confidence >= config.min_confidence);
            facts.sort_by(|a, b| {
                salience::score_fact(b, now)
                    .partial_cmp(&salience::score_fact(a, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            facts.truncate(config.max_facts);

            let event_pool = config.max_events.saturating_mul(5).max(50);
            let mut events = store.list_events(agent_id, event_pool)?;
            events.sort_by(|a, b| {
                salience::score_event(b, now)
                    .partial_cmp(&salience::score_event(a, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            events.truncate(config.max_events);

            let mut procedures = store.list_procedures(agent_id)?;
            procedures.sort_by(|a, b| {
                salience::score_procedure(b, now)
                    .partial_cmp(&salience::score_procedure(a, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            procedures.truncate(config.max_procedures);

            let mut budget = config.token_budget;
            let facts = budget_filter(facts, &mut budget, salience::estimate_fact_tokens);
            let events = budget_filter(events, &mut budget, salience::estimate_event_tokens);
            let procedures =
                budget_filter(procedures, &mut budget, salience::estimate_procedure_tokens);

            for fact in &facts {
                store.touch_fact(fact.agent_id, fact.fact_id)?;
            }

            Ok(MemoryPacket {
                facts,
                events,
                procedures,
            })
        })
        .await
        .map_err(|e| MemoryError::BlockingTaskFailed(e.to_string()))?
    }
}

/// Greedily select items that fit within the remaining token budget.
fn budget_filter<T>(items: Vec<T>, budget: &mut usize, estimator: fn(&T) -> usize) -> Vec<T> {
    let mut result = Vec::new();
    for item in items {
        let tokens = estimator(&item);
        if tokens > *budget {
            break;
        }
        *budget -= tokens;
        result.push(item);
    }
    result
}
