//! Salience scoring for memory retrieval prioritization.
//!
//! Scores combine importance, recency (exponential decay with ~7-day half-life),
//! and access frequency (log-scaled) to rank memory items for prompt injection.

use crate::types::{AgentEvent, Fact, Procedure};
use chrono::{DateTime, Utc};
use std::f32::consts::LN_2;

/// Score a fact using a weighted combination of importance, recency, and access frequency.
#[must_use]
pub fn score_fact(fact: &Fact, now: DateTime<Utc>) -> f32 {
    let recency = recency_decay(fact.last_accessed, now);
    let access = normalized_access(fact.access_count);
    0.2f32.mul_add(access, 0.5f32.mul_add(fact.importance, 0.3 * recency))
}

/// Score an event using importance and recency.
#[must_use]
pub fn score_event(event: &AgentEvent, now: DateTime<Utc>) -> f32 {
    let recency = recency_decay(event.timestamp, now);
    0.4f32.mul_add(event.importance, 0.6 * recency)
}

/// Score a procedure using success rate, recency, and execution frequency.
#[must_use]
pub fn score_procedure(proc: &Procedure, now: DateTime<Utc>) -> f32 {
    let recency = recency_decay(proc.last_used, now);
    let frequency = normalized_access(proc.execution_count);
    0.3f32.mul_add(frequency, 0.3f32.mul_add(recency, 0.4 * proc.success_rate))
}

/// Estimate token count for a string (bytes / 4 approximation).
#[must_use]
pub const fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Estimate the token cost of a fact's prompt representation.
#[must_use]
pub fn estimate_fact_tokens(fact: &Fact) -> usize {
    let val = match &fact.value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let line = format!(
        "- {}: {} (confidence: {:.2})",
        fact.key, val, fact.confidence
    );
    estimate_tokens(&line)
}

/// Estimate the token cost of an event's prompt representation.
#[must_use]
pub fn estimate_event_tokens(event: &AgentEvent) -> usize {
    let line = format!(
        "- [{}] {}: {}",
        event.timestamp.format("%Y-%m-%d"),
        event.event_type,
        event.summary
    );
    estimate_tokens(&line)
}

/// Estimate the token cost of a procedure's prompt representation.
#[must_use]
pub fn estimate_procedure_tokens(proc: &Procedure) -> usize {
    let steps = proc.steps.join(" -> ");
    let skill_tag = proc
        .skill_name
        .as_deref()
        .map(|s| format!(" [skill: {s}]"))
        .unwrap_or_default();
    let line = format!(
        "- \"{}\": {} (success: {:.0}%){skill_tag}",
        proc.name,
        steps,
        proc.success_rate * 100.0
    );
    estimate_tokens(&line)
}

/// Exponential decay based on time since last access.
/// Returns 1.0 for "just now", decaying toward 0.0 over time.
/// Half-life: ~7 days.
#[allow(clippy::cast_precision_loss)]
fn recency_decay(last_time: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let hours = (now - last_time).num_hours().max(0) as f32;
    let half_life_hours: f32 = 7.0 * 24.0;
    (-LN_2 * hours / half_life_hours).exp()
}

/// Normalize access count to 0.0..1.0 range using logarithmic scaling.
#[allow(clippy::cast_precision_loss)]
fn normalized_access(count: u32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let log_count = (count as f32).ln_1p();
    let log_max = 101.0_f32.ln();
    (log_count / log_max).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
    use chrono::{Duration, Utc};

    fn make_fact(importance: f32, access_count: u32, hours_ago: i64) -> Fact {
        let now = Utc::now();
        let last = now - Duration::hours(hours_ago);
        Fact {
            fact_id: FactId::generate(),
            agent_id: AgentId::generate(),
            key: "k".to_string(),
            value: serde_json::Value::String("v".to_string()),
            confidence: 0.9,
            source: crate::types::FactSource::Extracted,
            importance,
            access_count,
            last_accessed: last,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_event(importance: f32, hours_ago: i64) -> AgentEvent {
        let now = Utc::now();
        AgentEvent {
            event_id: AgentEventId::generate(),
            agent_id: AgentId::generate(),
            event_type: "t".to_string(),
            summary: "s".to_string(),
            metadata: serde_json::Value::Null,
            importance,
            access_count: 0,
            last_accessed: now,
            timestamp: now - Duration::hours(hours_ago),
        }
    }

    fn make_procedure(success_rate: f32, exec_count: u32, hours_ago: i64) -> Procedure {
        let now = Utc::now();
        Procedure {
            procedure_id: ProcedureId::generate(),
            agent_id: AgentId::generate(),
            name: "p".to_string(),
            trigger: "t".to_string(),
            steps: vec!["a".to_string()],
            context_constraints: serde_json::Value::Null,
            success_rate,
            execution_count: exec_count,
            last_used: now - Duration::hours(hours_ago),
            created_at: now,
            updated_at: now,
            skill_name: None,
            skill_relevance: None,
        }
    }

    #[test]
    fn recency_decay_same_instant() {
        let now = Utc::now();
        let val = recency_decay(now, now);
        assert!((val - 1.0).abs() < 1e-6);
    }

    #[test]
    fn recency_decay_large_gap_near_zero() {
        let now = Utc::now();
        let old = now - Duration::days(365);
        let val = recency_decay(old, now);
        assert!(val < 0.01);
    }

    #[test]
    fn recency_decay_monotonically_decreases() {
        let now = Utc::now();
        let v1 = recency_decay(now - Duration::hours(1), now);
        let v2 = recency_decay(now - Duration::hours(24), now);
        let v3 = recency_decay(now - Duration::hours(168), now);
        assert!(v1 > v2);
        assert!(v2 > v3);
    }

    #[test]
    fn normalized_access_zero() {
        assert_eq!(normalized_access(0), 0.0);
    }

    #[test]
    fn normalized_access_high_count_capped() {
        let val = normalized_access(10_000);
        assert!(val <= 1.0);
        assert!(val > 0.9);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short() {
        assert_eq!(estimate_tokens("abcd"), 1);
    }

    #[test]
    fn estimate_tokens_exact_multiple() {
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    #[test]
    fn estimate_tokens_partial() {
        assert_eq!(estimate_tokens("abc"), 1);
    }

    #[test]
    fn score_fact_finite() {
        let now = Utc::now();
        let f = make_fact(0.8, 5, 0);
        let s = score_fact(&f, now);
        assert!(s.is_finite());
        assert!(s > 0.0);
    }

    #[test]
    fn score_fact_more_important_ranks_higher() {
        let now = Utc::now();
        let high = make_fact(0.9, 5, 0);
        let low = make_fact(0.1, 5, 0);
        assert!(score_fact(&high, now) > score_fact(&low, now));
    }

    #[test]
    fn score_event_finite() {
        let now = Utc::now();
        let e = make_event(0.8, 0);
        let s = score_event(&e, now);
        assert!(s.is_finite());
        assert!(s > 0.0);
    }

    #[test]
    fn score_event_recent_ranks_higher() {
        let now = Utc::now();
        let recent = make_event(0.5, 1);
        let old = make_event(0.5, 720);
        assert!(score_event(&recent, now) > score_event(&old, now));
    }

    #[test]
    fn score_procedure_finite() {
        let now = Utc::now();
        let p = make_procedure(0.8, 10, 0);
        let s = score_procedure(&p, now);
        assert!(s.is_finite());
        assert!(s > 0.0);
    }

    #[test]
    fn estimate_fact_tokens_positive() {
        let f = make_fact(0.5, 0, 0);
        assert!(estimate_fact_tokens(&f) > 0);
    }

    #[test]
    fn estimate_event_tokens_positive() {
        let e = make_event(0.5, 0);
        assert!(estimate_event_tokens(&e) > 0);
    }

    #[test]
    fn estimate_procedure_tokens_positive() {
        let p = make_procedure(0.5, 1, 0);
        assert!(estimate_procedure_tokens(&p) > 0);
    }
}
