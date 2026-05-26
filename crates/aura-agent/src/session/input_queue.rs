//! Mid-task user steering buffer (Layer E.2).
//!
//! Codex's task shell pulls pending user input at the top of every
//! turn-loop iteration ([`codex-rs/core/src/session/turn.rs:218-222`](
//! https://github.com/.../codex-rs/core/src/session/turn.rs)) and
//! consults the same queue on turn break ([`regular.rs:84-85`](
//! https://github.com/.../codex-rs/core/src/tasks/regular.rs)) to
//! decide whether the task should run another turn. This module is
//! aura's analog: a session-scoped FIFO buffer of [`UserInput`]
//! variants that the [`AgentRunnerHandle`] writes into and the agent
//! loop drains on every turn boundary.
//!
//! # Module-level invariants (Rule 13)
//!
//! - **Single shared state**: the queue is owned via `Arc` and shared
//!   between the agent loop (drain side) and one or more
//!   [`AgentRunnerHandle`] clones (push side).
//! - **`has_pending` reflects the buffer**: every successful
//!   [`InputQueue::push`] flips the [`AtomicBool`] to `true`; every
//!   [`InputQueue::drain`] that empties the buffer flips it back to
//!   `false`. The flag is the lock-free probe the task shell uses to
//!   avoid acquiring the async mutex on every turn break.
//! - **Cancellation is in-band**: pushing [`UserInput::Cancel`]
//!   triggers the wrapped [`CancellationToken`] *and* enqueues the
//!   variant. The token is the unwind mechanism (Rule 6.3); the
//!   in-band variant gives the drain side a paper trail for tracing
//!   / observability.
//! - **Closed queues are typed errors**: once
//!   [`InputQueue::close`] runs, every subsequent
//!   [`InputQueue::push`] returns
//!   [`crate::AgentError::InputQueueClosed`] with the
//!   originating [`SessionId`] (Rules 4.1 / 4.3). Drains continue to
//!   succeed (the buffered remainder is returned) until the queue is
//!   empty.
//!
//! # Failure modes
//!
//! - The async mutex held across `.await` (drain returns a `Vec` so
//!   the lock is dropped before message-append work runs) is
//!   `tokio::sync::Mutex` (Rule 6.1).
//! - `push` is `async` but its critical section is `O(1)` and never
//!   awaits a foreign future. Lock contention is bounded by the
//!   number of concurrent handles (typically one per UI session).
//! - `drain` returns `Vec<UserInput>` rather than holding the lock
//!   while the caller appends to `state.messages` — this is the
//!   "atomic message append" guarantee from the plan: cancellation
//!   observed *between* drain and append never produces a half-write
//!   (`drain` already took the inputs out of the queue; either the
//!   append happens or the inputs are lost when the turn unwinds).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::AgentError;

use super::SessionId;

/// One unit of user input handed to the running task mid-flight.
///
/// The variants mirror codex's `InputItem` surface: free-form user
/// messages, in-band cancellation requests, and lightweight steering
/// directives that prepend an instruction without replacing the
/// preceding user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInput {
    /// Free-form user message. Appended to the conversation as a
    /// fresh `user` turn (or merged into the trailing `user` block
    /// when the conversation already ends with one — see
    /// [`crate::helpers::append_warning`] for the merge rule).
    Message(String),
    /// Request to unwind the active turn. Pushing this variant fires
    /// the [`CancellationToken`] paired with the queue (Rule 6.3),
    /// so the same `cancellation.cancelled()` branch that handles
    /// external Ctrl-C also handles in-band cancellation.
    Cancel,
    /// Mid-task steering note: appended *before* the next sampling
    /// request as additional context, but unlike [`Self::Message`]
    /// it is rendered with a `<harness_steer>` envelope so the
    /// model can distinguish a regular user reply from a directive
    /// inserted by the harness on the user's behalf.
    Steer {
        /// The steering instruction text rendered inside the
        /// `<harness_steer>` envelope.
        instruction: String,
    },
}

/// Session-scoped FIFO buffer of [`UserInput`] entries.
///
/// Held behind `Arc` so the agent loop (drain side) and the
/// [`AgentRunnerHandle`] (push side) share one instance. See the
/// module-level docs for invariants and failure modes.
pub(crate) struct InputQueue {
    /// Owning session id. Threaded into every
    /// [`AgentError::InputQueueClosed`] so the surfacing surface can
    /// correlate the failure with the session that produced it.
    session_id: SessionId,
    /// FIFO buffer guarded by a tokio mutex (Rule 6.1: the lock is
    /// taken from `async fn`s; `tokio::sync::Mutex` is required even
    /// though the critical section never `.await`s a foreign
    /// future).
    inner: Mutex<VecDeque<UserInput>>,
    /// Lock-free pending-check used by the task shell. Kept in sync
    /// with `inner.is_empty() == false` by every push / drain.
    has_pending: AtomicBool,
    /// Latched on [`Self::close`]. Subsequent pushes fail with
    /// [`AgentError::InputQueueClosed`]; drains continue to drain.
    closed: AtomicBool,
    /// Cancellation token shared with the agent loop. Pushing
    /// [`UserInput::Cancel`] fires this token so the active turn
    /// unwinds via the same code path as an external cancel.
    cancellation: CancellationToken,
}

impl InputQueue {
    /// Construct a fresh queue paired with the given cancellation
    /// token. The token is *not* owned by the queue — the caller
    /// keeps the original clone and is free to share it with other
    /// subsystems (e.g. the streaming pump that reacts to
    /// `cancellation.cancelled()` mid-flight).
    pub(crate) fn new(session_id: SessionId, cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            inner: Mutex::new(VecDeque::new()),
            has_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cancellation,
        })
    }

    /// Push a new input onto the back of the queue.
    ///
    /// Returns [`AgentError::InputQueueClosed`] when the queue has
    /// already been closed via [`Self::close`]. [`UserInput::Cancel`]
    /// additionally triggers the wrapped cancellation token; the
    /// cancel signal fires *before* the variant is dropped from the
    /// queue (in the early-close path) so external observers never
    /// see the queue empty while the token remains uncancelled.
    pub(crate) async fn push(&self, input: UserInput) -> Result<(), AgentError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AgentError::InputQueueClosed {
                session_id: self.session_id,
            });
        }
        let is_cancel = matches!(input, UserInput::Cancel);
        {
            let mut guard = self.inner.lock().await;
            guard.push_back(input);
        }
        self.has_pending.store(true, Ordering::Release);
        if is_cancel {
            debug!(
                session_id = %self.session_id,
                "UserInput::Cancel received; firing cancellation token"
            );
            self.cancellation.cancel();
        }
        Ok(())
    }

    /// Atomically take every pending input and return them in FIFO
    /// (push) order. The flag and buffer are cleared together so an
    /// observer that sees `has_pending == false` after the drain
    /// completes also sees the buffer empty.
    pub(crate) async fn drain(&self) -> Vec<UserInput> {
        let drained: Vec<UserInput> = {
            let mut guard = self.inner.lock().await;
            guard.drain(..).collect()
        };
        // Order matters: clear the flag *after* the buffer is empty.
        // A concurrent push would observe `closed == false`, lock the
        // mutex, push, then store `has_pending = true`, so the flag
        // remains a correct lower-bound on the buffer's emptiness.
        self.has_pending.store(false, Ordering::Release);
        drained
    }

    /// Lock-free `pending?` probe.
    ///
    /// Returns the current value of the atomic flag; the agent loop
    /// uses this between turns to decide whether to spin another
    /// turn before falling out of the task shell. A subsequent
    /// [`Self::drain`] re-checks the buffer under the mutex, so a
    /// race that flipped the flag back to `false` between probe and
    /// drain only costs one wasted turn boundary.
    pub(crate) fn has_pending(&self) -> bool {
        self.has_pending.load(Ordering::Acquire)
    }

    /// Mark the queue closed. Idempotent. Does not fire the
    /// cancellation token — callers that want both behaviours
    /// should push a [`UserInput::Cancel`] *then* close.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// The session this queue belongs to. Used by
    /// [`crate::session::Session::from_handle`] (Layer E.4) so a
    /// session built from an [`AgentRunnerHandle`] inherits the
    /// handle's session id verbatim.
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }
}

/// Public handle for queueing mid-task user input from outside the
/// agent loop.
///
/// Cheap to clone (one `Arc`); thread-safe; the same handle can be
/// shared across UI / RPC threads to deliver user input to a single
/// session. The handle owns the only public reference to the
/// underlying [`InputQueue`] — the queue itself stays `pub(crate)`
/// per Rule 3.1, exposed across the crate boundary only via this
/// type and the agent-loop entry point that reads from it.
///
/// The handle does NOT carry the session's [`CancellationToken`]
/// directly — call sites that need raw cancel access (e.g. the
/// existing chat / task runner code) keep using the token they
/// already have. The handle re-fires the same token from inside
/// [`Self::send_user_input`] when the user supplies
/// [`UserInput::Cancel`], so the two paths stay aligned.
#[derive(Clone)]
pub struct AgentRunnerHandle {
    queue: Arc<InputQueue>,
}

impl AgentRunnerHandle {
    /// Build a new handle. Internally constructs the backing
    /// [`InputQueue`] paired with `cancellation`; pass a clone of
    /// the same token to [`crate::AgentLoop::run_with_session`] so
    /// in-band cancel + external cancel share a single signal.
    #[must_use]
    pub fn new(session_id: SessionId, cancellation: CancellationToken) -> Self {
        Self {
            queue: InputQueue::new(session_id, cancellation),
        }
    }

    /// Construct a handle that shares an existing [`InputQueue`].
    ///
    /// Used by [`crate::session::Session::handle`] (Layer E.4) so a
    /// single session can hand out an arbitrary number of handles
    /// pointed at the same backing queue without re-creating the
    /// [`InputQueue`] / cancellation token from scratch.
    #[must_use]
    pub(crate) fn from_queue(queue: Arc<InputQueue>) -> Self {
        Self { queue }
    }

    /// Push a user input onto the running session's queue.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::InputQueueClosed`] with the originating
    /// [`SessionId`] when the queue has already been closed by the
    /// agent loop teardown.
    pub async fn send_user_input(&self, input: UserInput) -> Result<(), AgentError> {
        self.queue.push(input).await
    }

    /// Convenience: probe `has_pending` without taking the mutex.
    /// Mostly useful for unit tests of the public surface.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.queue.has_pending()
    }

    /// Close the underlying queue. Subsequent
    /// [`Self::send_user_input`] calls fail with
    /// [`AgentError::InputQueueClosed`]. Idempotent.
    pub fn close(&self) {
        self.queue.close();
    }

    /// Crate-internal accessor: clone the `Arc` into the agent
    /// loop's drain side. Kept `pub(crate)` so external callers
    /// cannot accidentally bypass the handle's send/close surface.
    pub(crate) fn queue(&self) -> Arc<InputQueue> {
        Arc::clone(&self.queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn drain_returns_inputs_in_fifo_order() {
        let cancel = CancellationToken::new();
        let queue = InputQueue::new(SessionId::new_v4(), cancel);
        queue.push(UserInput::Message("a".into())).await.unwrap();
        queue.push(UserInput::Message("b".into())).await.unwrap();
        queue
            .push(UserInput::Steer {
                instruction: "c".into(),
            })
            .await
            .unwrap();

        assert!(queue.has_pending());
        let drained = queue.drain().await;
        assert_eq!(drained.len(), 3);
        assert!(matches!(&drained[0], UserInput::Message(m) if m == "a"));
        assert!(matches!(&drained[1], UserInput::Message(m) if m == "b"));
        assert!(matches!(&drained[2], UserInput::Steer { instruction } if instruction == "c"));
        assert!(
            !queue.has_pending(),
            "flag must clear after a complete drain"
        );
    }

    #[tokio::test]
    async fn push_cancel_fires_token_and_enqueues_variant() {
        let cancel = CancellationToken::new();
        let queue = InputQueue::new(SessionId::new_v4(), cancel.clone());
        assert!(!cancel.is_cancelled());
        queue.push(UserInput::Cancel).await.unwrap();
        assert!(
            cancel.is_cancelled(),
            "UserInput::Cancel must fire the shared cancellation token"
        );
        let drained = queue.drain().await;
        assert_eq!(drained.len(), 1, "the cancel variant must also be enqueued");
        assert!(matches!(drained[0], UserInput::Cancel));
    }

    #[tokio::test]
    async fn push_after_close_returns_typed_error() {
        let cancel = CancellationToken::new();
        let session_id = SessionId::new_v4();
        let queue = InputQueue::new(session_id, cancel);
        queue.close();

        let err = queue
            .push(UserInput::Message("late".into()))
            .await
            .expect_err("push must fail after close");
        match err {
            AgentError::InputQueueClosed { session_id: got } => {
                assert_eq!(got, session_id, "error context must carry the session id");
            }
            other => panic!("expected InputQueueClosed, got {other:?}"),
        }
        assert!(!queue.has_pending());
    }

    #[tokio::test]
    async fn handle_send_user_input_propagates_through_queue() {
        let cancel = CancellationToken::new();
        let handle = AgentRunnerHandle::new(SessionId::new_v4(), cancel);
        handle
            .send_user_input(UserInput::Message("hello".into()))
            .await
            .unwrap();
        assert!(handle.has_pending());
        let queue = handle.queue();
        let drained = queue.drain().await;
        assert_eq!(drained.len(), 1);
    }

    #[tokio::test]
    async fn handle_close_idempotent_and_blocks_subsequent_sends() {
        let cancel = CancellationToken::new();
        let handle = AgentRunnerHandle::new(SessionId::new_v4(), cancel);
        handle.close();
        handle.close();
        let err = handle
            .send_user_input(UserInput::Message("x".into()))
            .await
            .expect_err("send must fail after handle close");
        assert!(matches!(err, AgentError::InputQueueClosed { .. }));
    }
}
