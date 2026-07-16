//! Request-body wire shapes consumed by the memory CRUD handlers.
//!
//! Kept separate from `handlers.rs` so a future schema change (e.g.
//! versioned bodies, new optional fields, alternate fact sources)
//! does not have to walk through several hundred lines of HTTP
//! plumbing to find the type definitions.

use aura_context_memory::{MemoryAccessContext, MemoryContinuity, MemoryScope};
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub(in crate::gateway) struct MemoryAccessParams {
    pub project_id: Option<String>,
    pub user_id: Option<String>,
    #[serde(default)]
    pub include_legacy: bool,
    pub scope: Option<MemoryScope>,
}

impl MemoryAccessParams {
    pub fn access(&self) -> MemoryAccessContext {
        MemoryAccessContext {
            project_id: self.project_id.clone().filter(|id| !id.trim().is_empty()),
            user_id: self.user_id.clone().filter(|id| !id.trim().is_empty()),
            include_legacy: self.include_legacy,
        }
    }
}

#[derive(Deserialize)]
pub(in crate::gateway) struct CreateFactBody {
    pub key: String,
    pub value: serde_json::Value,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default = "default_importance")]
    pub importance: f32,
    #[serde(default)]
    pub continuity: Option<MemoryContinuity>,
}

#[derive(Deserialize)]
pub(in crate::gateway) struct UpdateFactBody {
    pub key: Option<String>,
    pub value: Option<serde_json::Value>,
    pub confidence: Option<f32>,
    pub source: Option<String>,
    pub importance: Option<f32>,
    pub continuity: Option<MemoryContinuity>,
}

pub(in crate::gateway) fn default_confidence() -> f32 {
    1.0
}

pub(in crate::gateway) fn default_importance() -> f32 {
    0.5
}

#[derive(Deserialize)]
pub(in crate::gateway) struct CreateEventBody {
    pub event_type: String,
    pub summary: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default = "default_importance")]
    pub importance: f32,
    #[serde(default)]
    pub continuity: Option<MemoryContinuity>,
}

#[derive(Deserialize)]
pub(in crate::gateway) struct BulkDeleteEventsBody {
    pub before: DateTime<Utc>,
}

#[derive(Deserialize, Default)]
pub(in crate::gateway) struct ProcedureListParams {
    #[serde(flatten)]
    pub access: MemoryAccessParams,
    pub skill: Option<String>,
    pub min_relevance: Option<f32>,
}

#[derive(Deserialize)]
pub(in crate::gateway) struct CreateProcedureBody {
    pub name: String,
    pub trigger: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub context_constraints: serde_json::Value,
    #[serde(default)]
    pub skill_name: Option<String>,
    #[serde(default)]
    pub skill_relevance: Option<f32>,
    #[serde(default)]
    pub continuity: Option<MemoryContinuity>,
}

#[derive(Deserialize)]
pub(in crate::gateway) struct UpdateProcedureBody {
    pub name: Option<String>,
    pub trigger: Option<String>,
    pub steps: Option<Vec<String>>,
    pub context_constraints: Option<serde_json::Value>,
    pub skill_name: Option<String>,
    pub skill_relevance: Option<f32>,
    pub success_rate: Option<f32>,
    pub continuity: Option<MemoryContinuity>,
}
