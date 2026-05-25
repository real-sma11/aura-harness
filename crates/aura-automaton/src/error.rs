use thiserror::Error;

/// Errors from automaton lifecycle operations (install, tick, stop) and runtime management.
#[derive(Debug, Error)]
pub enum AutomatonError {
    #[error("automaton not found: {0}")]
    NotFound(String),

    #[error("automaton already running: {0}")]
    AlreadyRunning(String),

    #[error("automaton stopped: {0}")]
    Stopped(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("domain API error: {0}")]
    DomainApi(String),

    #[error("agent execution error: {0}")]
    AgentExecution(String),

    #[error("automaton event delivery failed: {0}")]
    EventDelivery(String),

    #[error("credits exhausted")]
    CreditsExhausted,

    /// Catch-all for unexpected conditions that don't fit a typed variant.
    /// Prefer adding a dedicated variant over introducing new call-sites here.
    #[error("unexpected: {0}")]
    Unexpected(String),
}
