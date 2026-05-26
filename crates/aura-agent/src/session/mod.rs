//! Session-scoped subsystems (Layer E.2+).
//!
//! A *session* is the long-lived container around one or more `Task`s
//! that an `AgentRunner` drives. E.2 introduces the first session-
//! scoped subsystem — the [`InputQueue`] — which lets the user steer
//! the agent mid-task without aborting the conversation. Subsequent
//! layers add the `GoalRuntime` (E.4) and the full `Session` struct
//! that owns both pieces; E.2 keeps the surface minimal so the
//! input-queue plumbing can land independently.
//!
//! ## Invariants enforced here
//!
//! - [`SessionId`] is a [`Uuid`] newtype (Rule 5.1) so error contexts
//!   never accept a raw string / `Uuid` by accident.
//! - All public types in this module carry `///` docs (Rule 3.3).
//! - The [`InputQueue`] is `pub(crate)`: external callers reach it
//!   only via the [`AgentRunnerHandle`] (Rule 3.1).
//!
//! ## Failure modes
//!
//! - [`InputQueue::push`] returns
//!   [`crate::AgentError::InputQueueClosed`] when the queue has been
//!   closed (e.g. the parent task has finished and the handle was
//!   shut down). This mirrors codex's `input_queue` behaviour on
//!   session teardown: silent drops would surface as ghost
//!   continuations later.
//! - [`UserInput::Cancel`] is the in-band cancellation signal: pushing
//!   one also fires the wrapped [`CancellationToken`] so the active
//!   turn unwinds via the same path as an external Ctrl-C (Rule 6.3).

pub(crate) mod input_queue;

use uuid::Uuid;

pub use input_queue::{AgentRunnerHandle, UserInput};

/// Newtype around [`Uuid`] identifying one in-flight agent session.
///
/// Generated per session (typically one per chat / per task-runner
/// instantiation) and threaded through the [`InputQueue`] so any
/// error originating from queue operations can attribute itself to
/// the session that produced it (Rule 4.3, Rule 5.1). E.4 will
/// thread the same id into [`crate::TaskId`] and the
/// `GoalRuntime` so a single session can correlate multiple tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Mint a fresh v4 session identifier.
    #[must_use]
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new_v4()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
