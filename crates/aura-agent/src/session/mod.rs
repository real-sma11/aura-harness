//! Session-scoped subsystems (Layer E.2 + E.4).
//!
//! A *session* is the long-lived container around one or more `Task`s
//! that an `AgentRunner` drives. E.2 introduced the first session-
//! scoped subsystem — the [`InputQueue`] — which lets the user steer
//! the agent mid-task without aborting the conversation. E.4 adds the
//! [`GoalRuntime`](goal_runtime::GoalRuntime) (out-of-loop
//! continuation driver, codex parity) and pulls both subsystems
//! behind a single [`Session`] handle that the agent loop borrows for
//! the lifetime of one or more tasks.
//!
//! ## Invariants enforced here
//!
//! - [`SessionId`] is a [`Uuid`] newtype (Rule 5.1) so error contexts
//!   never accept a raw string / `Uuid` by accident.
//! - All public types in this module carry `///` docs (Rule 3.3).
//! - The [`InputQueue`] and [`GoalRuntime`](goal_runtime::GoalRuntime)
//!   are `pub(crate)`: external callers reach them only via
//!   [`AgentRunnerHandle`] (Rule 3.1). The [`Session`] type itself is
//!   also `pub(crate)` — callers build one via
//!   [`Session::new`] and pass it through
//!   [`crate::AgentLoop::run_with_session`].
//!
//! ## Failure modes
//!
//! - [`InputQueue::push`](input_queue::InputQueue::push) returns
//!   [`crate::AgentError::InputQueueClosed`] when the queue has been
//!   closed (e.g. the parent task has finished and the handle was
//!   shut down). This mirrors codex's `input_queue` behaviour on
//!   session teardown: silent drops would surface as ghost
//!   continuations later.
//! - [`UserInput::Cancel`] is the in-band cancellation signal: pushing
//!   one also fires the wrapped [`CancellationToken`] so the active
//!   turn unwinds via the same path as an external Ctrl-C (Rule 6.3).
//! - The [`GoalRuntime`](goal_runtime::GoalRuntime) propagates
//!   [`AgentError`](crate::AgentError) through `?` — never panics on
//!   `max_continuation_turns` overflow; instead returns a typed
//!   `TaskRestart::Blocked` so the caller can mark the task
//!   `stalled` / `task_blocked`.

pub(crate) mod goal_runtime;
pub(crate) mod input_queue;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use input_queue::{AgentRunnerHandle, UserInput};

pub(crate) use goal_runtime::GoalRuntime;
pub(crate) use input_queue::InputQueue;

/// Newtype around [`Uuid`] identifying one in-flight agent session.
///
/// Generated per session (typically one per chat / per task-runner
/// instantiation) and threaded through the [`InputQueue`] so any
/// error originating from queue operations can attribute itself to
/// the session that produced it (Rule 4.3, Rule 5.1). E.4 also
/// threads it through every [`GoalRuntime`] decision so the
/// `task_blocked` escalation carries a session attribution.
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

/// One agent session: owns the [`InputQueue`] and the
/// [`GoalRuntime`] for the lifetime of one or more tasks.
///
/// `Session` itself is `pub(crate)` (Rule 3.1): external callers
/// build it via [`Session::new`] from outside the crate by way of the
/// new agent-runner-side instantiation path
/// (`agent_runner::execute_task`), then hand it to
/// [`crate::AgentLoop::run_with_session`]. The public surface stays
/// shaped around [`AgentRunnerHandle`] — see
/// [`Session::handle`] for the cheap clonable handle.
pub(crate) struct Session {
    /// Stable session identifier (Rule 5.1).
    pub(crate) id: SessionId,
    /// Mid-task user steering buffer. Shared with
    /// [`AgentRunnerHandle`] clones.
    pub(crate) input_queue: Arc<InputQueue>,
    /// Out-of-loop continuation driver (E.4). Shared with the agent
    /// loop via [`Arc`] so the turn stop hook can call
    /// [`GoalRuntime::handle_event`] without taking ownership.
    pub(crate) goal_runtime: Arc<GoalRuntime>,
    /// Cancellation token shared with the [`InputQueue`] and the
    /// agent loop. Cloned on construction so the caller's original
    /// token stays usable.
    #[allow(dead_code)]
    // Held to keep the token alive for the session lifetime; consumed by future idle handler.
    pub(crate) cancellation: CancellationToken,
}

impl Session {
    /// Construct a fresh session.
    ///
    /// `max_continuation_turns` is the hard ceiling on the number of
    /// continuation prompts the [`GoalRuntime`] will inject before
    /// escalating to `task_blocked`; forward
    /// [`crate::AgentLoopConfig::max_continuation_turns`] verbatim.
    pub(crate) fn new(
        id: SessionId,
        cancellation: CancellationToken,
        max_continuation_turns: u32,
    ) -> Self {
        let input_queue = InputQueue::new(id, cancellation.clone());
        let goal_runtime = Arc::new(GoalRuntime::new(id, max_continuation_turns));
        Self {
            id,
            input_queue,
            goal_runtime,
            cancellation,
        }
    }

    /// Construct a session that shares the [`InputQueue`] backing
    /// the supplied [`AgentRunnerHandle`].
    ///
    /// Used by [`crate::AgentLoop::run_with_session`] so a single
    /// session can be built from a caller-supplied handle without
    /// re-creating the underlying queue (which would split user
    /// inputs across two FIFOs). The queue's own cancellation token
    /// stays in place; the session-level `cancellation` parameter is
    /// the agent-loop / external observer (cloned so the caller's
    /// original token stays usable).
    pub(crate) fn from_handle(
        handle: &AgentRunnerHandle,
        cancellation: CancellationToken,
        max_continuation_turns: u32,
    ) -> Self {
        let input_queue = handle.queue();
        let id = input_queue.session_id();
        let goal_runtime = Arc::new(GoalRuntime::new(id, max_continuation_turns));
        Self {
            id,
            input_queue,
            goal_runtime,
            cancellation,
        }
    }

    /// Cheap clonable handle for queueing user input from outside the
    /// agent loop. The handle shares the underlying [`InputQueue`]
    /// with the session.
    #[allow(dead_code)] // Constructed for symmetry; surfaces via `Session::handle()` in follow-up.
    pub(crate) fn handle(&self) -> AgentRunnerHandle {
        AgentRunnerHandle::from_queue(Arc::clone(&self.input_queue))
    }
}
