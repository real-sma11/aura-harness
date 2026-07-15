//! Core memory types.

use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// Visibility boundary for a memory.
///
/// `Agent` means one agent inside one project, `Project` is shared by the
/// project's approved agents, and `User` follows the user across projects and
/// agents. The storage namespace is derived from the active context rather
/// than from this enum alone.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    #[default]
    Agent,
    User,
    #[serde(alias = "workspace")]
    Project,
}

/// Identity boundary used to derive isolated RocksDB partitions.
///
/// The IDs are intentionally kept as opaque strings. `storage_id` hashes
/// them into the existing 32-byte `AgentId` key prefix, which lets the v2
/// scope model remain backward-compatible with the existing column families.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryAccessContext {
    pub project_id: Option<String>,
    pub user_id: Option<String>,
    /// Management surfaces may show legacy, unscoped v1 records for review.
    /// Runtime prompt retrieval never enables this flag.
    pub include_legacy: bool,
}

impl MemoryAccessContext {
    #[must_use]
    pub fn storage_id(&self, agent_id: AgentId, scope: MemoryScope) -> AgentId {
        let missing_scope_identity = match scope {
            MemoryScope::Agent | MemoryScope::Project => self.project_id.is_none(),
            MemoryScope::User => self.user_id.is_none(),
        };
        if missing_scope_identity {
            return agent_id;
        }
        let namespace = match scope {
            MemoryScope::Agent => format!(
                "project-agent:{}:{}",
                self.project_id.as_deref().unwrap_or_default(),
                agent_id.to_hex()
            ),
            MemoryScope::Project => {
                format!("project:{}", self.project_id.as_deref().unwrap_or_default())
            }
            MemoryScope::User => {
                format!("user:{}", self.user_id.as_deref().unwrap_or_default())
            }
        };
        let digest = blake3::hash(format!("aura-memory-v2:{namespace}").as_bytes());
        AgentId::new(*digest.as_bytes())
    }

    #[must_use]
    pub fn readable_partitions(
        &self,
        agent_id: AgentId,
        allow_user_scope: bool,
        allow_project_scope: bool,
    ) -> Vec<AgentId> {
        // Project-less callers preserve the v1 agent bucket. Project-aware
        // callers fail closed and never read it unless a management surface
        // explicitly asks to review legacy records. Keep narrow-to-broad
        // precedence for same-key lookups instead of sorting opaque hashes.
        let mut partitions = if self.project_id.is_some() {
            vec![self.storage_id(agent_id, MemoryScope::Agent)]
        } else {
            vec![agent_id]
        };
        if allow_project_scope && self.project_id.is_some() {
            let partition = self.storage_id(agent_id, MemoryScope::Project);
            if !partitions.contains(&partition) {
                partitions.push(partition);
            }
        }
        if allow_user_scope && self.user_id.is_some() {
            let partition = self.storage_id(agent_id, MemoryScope::User);
            if !partitions.contains(&partition) {
                partitions.push(partition);
            }
        }
        if self.include_legacy && self.project_id.is_some() && !partitions.contains(&agent_id) {
            partitions.push(agent_id);
        }
        partitions
    }
}

/// Lifecycle state for a memory record.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Pending,
    Rejected,
    Superseded,
}

/// Coarse sensitivity label used by automatic-write policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySensitivity {
    #[default]
    Normal,
    Sensitive,
}

/// Local evidence explaining where a memory came from. Evidence never leaves
/// the memory store through product analytics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributor_agent_id: Option<String>,
}

/// Backward-compatible metadata shared by every memory type.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryContinuity {
    #[serde(default)]
    pub scope: MemoryScope,
    #[serde(default)]
    pub status: MemoryStatus,
    #[serde(default)]
    pub sensitivity: MemorySensitivity,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub provenance: MemoryProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWritePolicy {
    #[default]
    Automatic,
    Approval,
    ExplicitOnly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRetrievalMode {
    Salience,
    #[default]
    QueryAware,
}

/// Persisted, per-agent controls for the Agent Continuity system.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentContinuityConfig {
    #[serde(default = "default_true")]
    pub use_memory: bool,
    #[serde(default = "default_true")]
    pub generate_memory: bool,
    #[serde(default)]
    pub write_policy: MemoryWritePolicy,
    #[serde(default)]
    pub retrieval_mode: MemoryRetrievalMode,
    #[serde(default = "default_true")]
    pub allow_user_scope: bool,
    #[serde(default = "default_true", alias = "allow_workspace_scope")]
    pub allow_project_scope: bool,
}

impl Default for AgentContinuityConfig {
    fn default() -> Self {
        Self {
            use_memory: true,
            generate_memory: true,
            write_policy: MemoryWritePolicy::Automatic,
            retrieval_mode: MemoryRetrievalMode::QueryAware,
            allow_user_scope: true,
            allow_project_scope: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

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
    #[serde(default)]
    pub continuity: MemoryContinuity,
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
    #[serde(default)]
    pub continuity: MemoryContinuity,
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
    #[serde(default)]
    pub continuity: MemoryContinuity,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryPacket {
    pub facts: Vec<Fact>,
    pub events: Vec<AgentEvent>,
    pub procedures: Vec<Procedure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<MemoryRetrievalTrace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemorySelection {
    pub memory_id: String,
    pub kind: String,
    pub score: f32,
    pub relevance: f32,
    pub reason: String,
    pub scope: MemoryScope,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryRetrievalTrace {
    pub candidate_count: usize,
    pub selected_count: usize,
    pub estimated_tokens: usize,
    pub duration_ms: u64,
    pub query_aware: bool,
    pub selections: Vec<MemorySelection>,
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

        let mut out = String::from(
            "\n<agent_memory>\nOnly use memories that are relevant to the current request. Memory IDs are local provenance references.\n",
        );

        if !self.facts.is_empty() {
            out.push_str("<facts>\n");
            for fact in &self.facts {
                let val = match &fact.value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let _ = writeln!(
                    out,
                    "- [memory:{}] {}: {} (confidence: {:.2}, scope: {:?})",
                    fact.fact_id.to_hex(),
                    fact.key,
                    val,
                    fact.confidence,
                    fact.continuity.scope
                );
            }
            out.push_str("</facts>\n");
        }

        if !self.events.is_empty() {
            out.push_str("<recent_events>\n");
            for event in &self.events {
                let _ = writeln!(
                    out,
                    "- [memory:{}] [{}] {}: {}",
                    event.event_id.to_hex(),
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
                    "- [memory:{}] \"{}\": {} (success: {:.0}%){skill_tag}",
                    proc.procedure_id.to_hex(),
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
    #[serde(default)]
    pub scope: MemoryScope,
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
    use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
    use chrono::Utc;

    #[test]
    fn readable_partitions_keep_narrow_to_broad_precedence() {
        let agent_id = AgentId::generate();
        let access = MemoryAccessContext {
            project_id: Some("project-1".to_string()),
            user_id: Some("user-1".to_string()),
            include_legacy: true,
        };

        assert_eq!(
            access.readable_partitions(agent_id, true, true),
            vec![
                access.storage_id(agent_id, MemoryScope::Agent),
                access.storage_id(agent_id, MemoryScope::Project),
                access.storage_id(agent_id, MemoryScope::User),
                agent_id,
            ]
        );
    }

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
            continuity: MemoryContinuity::default(),
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
            continuity: MemoryContinuity::default(),
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
            continuity: MemoryContinuity::default(),
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
            continuity: MemoryContinuity::default(),
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
            trace: None,
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
