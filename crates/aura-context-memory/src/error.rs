//! Memory subsystem error types.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("store error: {0}")]
    Store(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("fact not found: agent={agent_id}, fact={fact_id}")]
    FactNotFound { agent_id: String, fact_id: String },

    #[error("event not found: agent={agent_id}, event={event_id}")]
    EventNotFound { agent_id: String, event_id: String },

    #[error("procedure not found: agent={agent_id}, procedure={procedure_id}")]
    ProcedureNotFound {
        agent_id: String,
        procedure_id: String,
    },

    #[error("column family not found: {0}")]
    ColumnFamilyNotFound(String),

    #[error("refinement error: {0}")]
    Refinement(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("blocking task failed: {0}")]
    BlockingTaskFailed(String),
}

impl From<serde_json::Error> for MemoryError {
    fn from(err: serde_json::Error) -> Self {
        if err.classify() == serde_json::error::Category::Data
            || err.classify() == serde_json::error::Category::Eof
        {
            Self::Deserialization(err.to_string())
        } else {
            Self::Serialization(err.to_string())
        }
    }
}

impl From<rocksdb::Error> for MemoryError {
    fn from(err: rocksdb::Error) -> Self {
        Self::Store(err.to_string())
    }
}

impl MemoryError {
    /// Returns `true` when this error indicates a missing entity.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::FactNotFound { .. } | Self::EventNotFound { .. } | Self::ProcedureNotFound { .. }
        )
    }
}
