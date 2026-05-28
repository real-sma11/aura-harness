//! Error types for the skill system.

use thiserror::Error;

/// All errors that can occur in skill parsing, loading, activation, or registry ops.
#[derive(Error, Debug)]
pub enum SkillError {
    /// A skill with the given name was not found in the registry.
    #[error("skill not found: {0}")]
    NotFound(String),

    /// Failed to parse a SKILL.md file (bad frontmatter, missing delimiters, etc.).
    #[error("parse error: {0}")]
    Parse(String),

    /// The skill name violates naming constraints (lowercase, hyphens, digits, 1-64 chars).
    #[error("invalid skill name: {0}")]
    InvalidName(String),

    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// YAML deserialization failure in frontmatter.
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// Failure during skill activation (argument substitution, rendering, etc.).
    #[error("activation error: {0}")]
    Activation(String),

    /// Shell command execution failed during backtick command injection.
    #[error("command execution error: {0}")]
    CommandExecution(String),

    /// Persistent storage failure (RocksDB column family or I/O).
    #[error("store error: {0}")]
    Store(String),

    /// A `SKILL.md` file exceeded the maximum allowed size (Wave 5 / T4).
    #[error("skill too large: {path} is {actual} bytes, limit {limit} bytes")]
    TooLarge {
        path: std::path::PathBuf,
        actual: u64,
        limit: u64,
    },
}

impl SkillError {
    /// Returns `true` when this error indicates a missing entity.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}
