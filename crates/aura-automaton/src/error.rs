use aura_agent::AgentError;
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

    /// Domain API call failed. `task_id` is `Some` when the failure is
    /// attributable to a specific task (dev-loop, task-run) and `None`
    /// for project- or chat-scoped operations.
    ///
    /// The inner `source` is the originating `anyhow::Error` returned by
    /// the `DomainApi` trait. We preserve it via `#[source]` so callers
    /// can walk the chain (Rule 4.3) instead of reparsing the
    /// stringified message.
    #[error(
        "domain API error{}: {source}",
        fmt_task_scope(task_id.as_deref())
    )]
    DomainApi {
        task_id: Option<String>,
        #[source]
        source: anyhow::Error,
    },

    /// Agent loop / sampling call failed under this automaton's tick.
    /// `task_id` is `Some` when the failure is attributable to a
    /// specific task and `None` for chat-shaped or auxiliary calls
    /// (e.g. spec generation).
    ///
    /// Carries the typed [`AgentError`] via `#[source]` so callers can
    /// branch on `Reason` / `TurnBudgetExceeded` / `StreamTimeout` /
    /// `BuildFailed` without reparsing the formatted message
    /// (Rule 4.3). `AgentError` is `Box`ed because it is significantly
    /// larger than the other variants here and would otherwise inflate
    /// the size of every `Result<_, AutomatonError>` (`clippy::result_large_err`).
    #[error(
        "agent execution error{}: {source}",
        fmt_task_scope(task_id.as_deref())
    )]
    AgentExecution {
        task_id: Option<String>,
        #[source]
        source: Box<AgentError>,
    },

    #[error("automaton event delivery failed: {0}")]
    EventDelivery(String),

    /// Serializing or deserializing typed values into/out of the
    /// untyped [`crate::AutomatonState`] JSON bag failed. Carries the
    /// state key and the originating `serde_json::Error` via
    /// `#[source]` so callers can chase the root cause (Rule 4.3).
    ///
    /// Previously the bag silently swallowed serialization errors on
    /// `set`, leaving the automaton with a state that was missing the
    /// value it claimed to have written. Surfacing it as a typed
    /// variant lets the dev-loop / task-run tick decide whether to
    /// abort or recover.
    #[error("automaton state serialization failed for key {key:?}: {source}")]
    StateSerialization {
        key: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("credits exhausted")]
    CreditsExhausted,
}

impl AutomatonError {
    /// Build a typed [`AutomatonError::AgentExecution`] from an
    /// `AgentError` and an optional task scope. Centralises the
    /// `Box<AgentError>` packaging so call sites stay terse.
    pub(crate) fn agent_execution(task_id: Option<String>, source: AgentError) -> Self {
        Self::AgentExecution {
            task_id,
            source: Box::new(source),
        }
    }

    /// Build a typed [`AutomatonError::DomainApi`] from an
    /// `anyhow::Error` and an optional task scope.
    pub(crate) fn domain_api(task_id: Option<String>, source: anyhow::Error) -> Self {
        Self::DomainApi { task_id, source }
    }
}

/// Format helper for the `AgentExecution` / `DomainApi` variants' Display.
/// Returns `" (task <id>)"` when a task scope is known, or an empty
/// string otherwise, so the human-readable error keeps reading naturally
/// in both cases.
fn fmt_task_scope(task_id: Option<&str>) -> String {
    match task_id {
        Some(id) => format!(" (task {id})"),
        None => String::new(),
    }
}
