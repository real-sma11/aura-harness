//! Async process types: pending processes and action result payloads.

use crate::ids::{ActionId, ProcessId};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Payload for a pending process effect.
///
/// This is stored in the Effect payload when a command exceeds the sync threshold
/// and is moved to async execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessPending {
    /// Unique process identifier for tracking
    pub process_id: ProcessId,
    /// The command being executed
    pub command: String,
    /// When the process started (milliseconds since epoch)
    pub started_at_ms: u64,
}

impl ProcessPending {
    /// Create a new pending process payload.
    #[must_use]
    pub fn new(process_id: ProcessId, command: impl Into<String>) -> Self {
        let started_at_ms = crate::time::now_ms();

        Self {
            process_id,
            command: command.into(),
            started_at_ms,
        }
    }
}

/// Payload for `ActionResult` transactions from completed async processes.
///
/// This is used when an async process completes and needs to be recorded
/// as a continuation of the original transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResultPayload {
    /// The `action_id` this result continues
    pub action_id: ActionId,
    /// Process identifier for correlation
    pub process_id: ProcessId,
    /// Exit code from the process
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Standard output from the process
    #[serde(default, with = "crate::serde_helpers::bytes_serde")]
    pub stdout: Bytes,
    /// Standard error from the process
    #[serde(default, with = "crate::serde_helpers::bytes_serde")]
    pub stderr: Bytes,
    /// Whether the process succeeded
    pub success: bool,
    /// Duration in milliseconds
    pub duration_ms: u64,
}

impl ActionResultPayload {
    /// Create a successful result payload.
    #[must_use]
    pub fn success(
        action_id: ActionId,
        process_id: ProcessId,
        exit_code: Option<i32>,
        stdout: impl Into<Bytes>,
        duration_ms: u64,
    ) -> Self {
        Self {
            action_id,
            process_id,
            exit_code,
            stdout: stdout.into(),
            stderr: Bytes::new(),
            success: true,
            duration_ms,
        }
    }

    /// Create a failed result payload.
    #[must_use]
    pub fn failure(
        action_id: ActionId,
        process_id: ProcessId,
        exit_code: Option<i32>,
        stderr: impl Into<Bytes>,
        duration_ms: u64,
    ) -> Self {
        Self {
            action_id,
            process_id,
            exit_code,
            stdout: Bytes::new(),
            stderr: stderr.into(),
            success: false,
            duration_ms,
        }
    }
}
