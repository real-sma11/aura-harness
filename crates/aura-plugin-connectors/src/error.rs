//! Error type for the connector registry.

use thiserror::Error;

/// Reasons a connector-registry operation can fail.
#[derive(Debug, Error)]
pub enum ConnectorError {
    /// Another plugin already registered this connector id.
    /// First-contributor-wins; the caller may warn-log and continue.
    #[error("connector id `{0}` already registered")]
    AlreadyRegistered(String),
    /// Lookup miss.
    #[error("unknown connector id `{0}`")]
    UnknownConnector(String),
}
