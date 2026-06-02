//! # Runtime
//!
//! Process manager used by `aura-agent`.
//!
//! This module provides:
//! - Process manager for async command execution

pub mod process_manager;

pub use process_manager::{ProcessManager, ProcessManagerConfig, ProcessOutput, RunningProcess};

/// Errors from the process manager runtime layer.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("model error: {0}")]
    Model(String),
    #[error("tool execution error: {0}")]
    ToolExecution(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("{0}")]
    Internal(String),
}

impl From<aura_model_reasoner::ReasonerError> for RuntimeError {
    fn from(e: aura_model_reasoner::ReasonerError) -> Self {
        match e {
            aura_model_reasoner::ReasonerError::Timeout => {
                Self::Timeout("model request timed out".to_string())
            }
            aura_model_reasoner::ReasonerError::InsufficientCredits(msg) => {
                Self::Model(format!("insufficient credits: {msg}"))
            }
            aura_model_reasoner::ReasonerError::RateLimited { message, .. } => {
                Self::Model(format!("rate limited: {message}"))
            }
            aura_model_reasoner::ReasonerError::Transient {
                status, message, ..
            }
            | aura_model_reasoner::ReasonerError::Api { status, message } => {
                Self::Model(format!("api error ({status}): {message}"))
            }
            aura_model_reasoner::ReasonerError::Request(msg) => {
                Self::Model(format!("request error: {msg}"))
            }
            aura_model_reasoner::ReasonerError::Parse(msg) => {
                Self::Model(format!("parse error: {msg}"))
            }
            aura_model_reasoner::ReasonerError::Internal(msg) => Self::Model(msg),
            // Exhausted per-tool-call streaming retries: surface the
            // classified reason through `Model(..)` so the outer
            // loop / server treats it the same as any other
            // provider error (credit accounting, task retry, etc.).
            aura_model_reasoner::ReasonerError::StreamAbortedWithPartial { reason, .. } => {
                Self::Model(reason)
            }
            aura_model_reasoner::ReasonerError::ModelRequestContractViolation(violation) => {
                Self::Model(violation.to_string())
            }
        }
    }
}
