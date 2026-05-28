//! Core memory types.

use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub fact_id: FactId,
    pub agent_id: AgentId,
    pub key: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub source: FactSource,
    pub importance: f32,
    pub access_count: u32,
    pub last_accessed: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    Extracted,
    UserProvided,
    Consolidated,
}

impl std::fmt::Display for FactSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extracted => write!(f, "extracted"),
            Self::UserProvided => write!(f, "user_provided"),
            Self::Consolidated => write!(f, "consolidated"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub event_id: AgentEventId,
    pub agent_id: AgentId,
    pub event_type: String,
    pub summary: String,
    pub metadata: serde_json::Value,
    pub importance: f32,
    pub access_count: u32,
    pub last_accessed: DateTime<Utc>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Procedure {
    pub procedure_id: ProcedureId,
    pub agent_id: AgentId,
    pub name: String,
    pub trigger: String,
    pub steps: Vec<String>,
    pub context_constraints: serde_json::Value,
    pub success_rate: f32,
    pub execution_count: u32,
    pub last_used: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Skill that was active when this procedure was learned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    /// How relevant this procedure is to the associated skill (0.0–1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_relevance: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryPacket {
    pub facts: Vec<Fact>,
    pub events: Vec<AgentEvent>,
    pub procedures: Vec<Procedure>,
}

impl MemoryPacket {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.events.is_empty() && self.procedures.is_empty()
    }

    #[must_use]
    pub fn format_for_prompt(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let mut out = String::from("\n<agent_memory>\n");

        if !self.facts.is_empty() {
            out.push_str("<facts>\n");
            for fact in &self.facts {
                let val = match &fact.value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let _ = writeln!(
                    out,
                    "- {}: {} (confidence: {:.2})",
                    fact.key, val, fact.confidence
                );
            }
            out.push_str("</facts>\n");
        }

        if !self.events.is_empty() {
            out.push_str("<recent_events>\n");
            for event in &self.events {
                let _ = writeln!(
                    out,
                    "- [{}] {}: {}",
                    event.timestamp.format("%Y-%m-%d"),
                    event.event_type,
                    event.summary
                );
            }
            out.push_str("</recent_events>\n");
        }

        if !self.procedures.is_empty() {
            out.push_str("<procedures>\n");
            for proc in &self.procedures {
                let steps = proc.steps.join(" -> ");
                let skill_tag = proc
                    .skill_name
                    .as_deref()
                    .map(|s| format!(" [skill: {s}]"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "- \"{}\": {} (success: {:.0}%){skill_tag}",
                    proc.name,
                    steps,
                    proc.success_rate * 100.0
                );
            }
            out.push_str("</procedures>\n");
        }

        out.push_str("</agent_memory>");
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CandidateType {
    Fact,
    Event,
    Procedure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub candidate_type: CandidateType,
    pub key: Option<String>,
    pub value: serde_json::Value,
    pub summary: Option<String>,
    pub source_hint: String,
    pub preliminary_confidence: f32,
    pub preliminary_importance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedCandidate {
    pub candidate_type: CandidateType,
    pub key: String,
    pub value: serde_json::Value,
    pub summary: Option<String>,
    pub confidence: f32,
    pub importance: f32,
    pub keep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
    use chrono::Utc;

    fn make_fact(key: &str, val: &str) -> Fact {
        let now = Utc::now();
        Fact {
            fact_id: FactId::generate(),
            agent_id: AgentId::generate(),
            key: key.to_string(),
            value: serde_json::Value::String(val.to_string()),
            confidence: 0.9,
            source: FactSource::Extracted,
            importance: 0.5,
            access_count: 0,
            last_accessed: now,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_event(event_type: &str, summary: &str) -> AgentEvent {
        let now = Utc::now();
        AgentEvent {
            event_id: AgentEventId::generate(),
            agent_id: AgentId::generate(),
            event_type: event_type.to_string(),
            summary: summary.to_string(),
            metadata: serde_json::Value::Null,
            importance: 0.6,
            access_count: 0,
            last_accessed: now,
            timestamp: now,
        }
    }

    fn make_procedure(name: &str, steps: Vec<&str>) -> Procedure {
        let now = Utc::now();
        Procedure {
            procedure_id: ProcedureId::generate(),
            agent_id: AgentId::generate(),
            name: name.to_string(),
            trigger: "test".to_string(),
            steps: steps.into_iter().map(String::from).collect(),
            context_constraints: serde_json::Value::Null,
            success_rate: 0.8,
            execution_count: 5,
            last_used: now,
            created_at: now,
            updated_at: now,
            skill_name: None,
            skill_relevance: None,
        }
    }

    #[test]
    fn is_empty_default() {
        let p = MemoryPacket::default();
        assert!(p.is_empty());
    }

    #[test]
    fn is_empty_facts_only() {
        let p = MemoryPacket {
            facts: vec![make_fact("k", "v")],
            ..Default::default()
        };
        assert!(!p.is_empty());
    }

    #[test]
    fn is_empty_events_only() {
        let p = MemoryPacket {
            events: vec![make_event("t", "s")],
            ..Default::default()
        };
        assert!(!p.is_empty());
    }

    #[test]
    fn is_empty_procedures_only() {
        let p = MemoryPacket {
            procedures: vec![make_procedure("p", vec!["a", "b"])],
            ..Default::default()
        };
        assert!(!p.is_empty());
    }

    #[test]
    fn format_for_prompt_empty() {
        let p = MemoryPacket::default();
        assert_eq!(p.format_for_prompt(), "");
    }

    #[test]
    fn format_for_prompt_facts_only() {
        let p = MemoryPacket {
            facts: vec![make_fact("lang", "Rust")],
            ..Default::default()
        };
        let out = p.format_for_prompt();
        assert!(out.contains("<agent_memory>"));
        assert!(out.contains("<facts>"));
        assert!(out.contains("lang: Rust"));
        assert!(out.contains("confidence: 0.90"));
        assert!(!out.contains("<recent_events>"));
        assert!(!out.contains("<procedures>"));
    }

    #[test]
    fn format_for_prompt_non_string_value() {
        let now = Utc::now();
        let fact = Fact {
            fact_id: FactId::generate(),
            agent_id: AgentId::generate(),
            key: "count".to_string(),
            value: serde_json::json!(42),
            confidence: 0.8,
            source: FactSource::Extracted,
            importance: 0.5,
            access_count: 0,
            last_accessed: now,
            created_at: now,
            updated_at: now,
        };
        let p = MemoryPacket {
            facts: vec![fact],
            ..Default::default()
        };
        let out = p.format_for_prompt();
        assert!(out.contains("count: 42"));
    }

    #[test]
    fn format_for_prompt_mixed() {
        let p = MemoryPacket {
            facts: vec![make_fact("k", "v")],
            events: vec![make_event("task_run", "did stuff")],
            procedures: vec![make_procedure("deploy", vec!["build", "push"])],
        };
        let out = p.format_for_prompt();
        assert!(out.contains("<facts>"));
        assert!(out.contains("<recent_events>"));
        assert!(out.contains("<procedures>"));
        assert!(out.contains("build -> push"));
    }

    #[test]
    fn fact_source_display() {
        assert_eq!(FactSource::Extracted.to_string(), "extracted");
        assert_eq!(FactSource::UserProvided.to_string(), "user_provided");
        assert_eq!(FactSource::Consolidated.to_string(), "consolidated");
    }

    #[test]
    fn fact_serde_roundtrip() {
        let f = make_fact("key", "value");
        let json = serde_json::to_string(&f).unwrap();
        let parsed: Fact = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.key, "key");
    }

    #[test]
    fn event_serde_roundtrip() {
        let e = make_event("task_run", "summary");
        let json = serde_json::to_string(&e).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_type, "task_run");
    }

    #[test]
    fn procedure_serde_roundtrip() {
        let p = make_procedure("deploy", vec!["build", "push"]);
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Procedure = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.steps.len(), 2);
    }
}
