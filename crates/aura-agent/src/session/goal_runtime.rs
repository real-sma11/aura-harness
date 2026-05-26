//! Out-of-loop goal continuation runtime (Layer E.4).
//!
//! Codex's `maybe_start_goal_continuation_turn`
//! ([codex-rs/core/src/goals.rs:1289-1357](
//! https://github.com/.../codex-rs/core/src/goals.rs)) is invoked on
//! session-idle events rather than from inside the per-iteration
//! turn loop. This module is aura's analog: a session-scoped runtime
//! that observes turn boundaries (and, in a follow-up, true
//! session-idle events) and decides whether the running task should
//! be steered with another continuation prompt or escalated to
//! `task_blocked`.
//!
//! It supersedes the pre-E.4 in-loop helper in
//! [`crate::agent_loop::continuation`] — that module is now a
//! `pub(crate)` re-export shim so external callers keep compiling
//! during the transition.
//!
//! # Module-level invariants (Rule 13)
//!
//! - **Single active goal per session**: [`GoalRuntime::active_goal`]
//!   ever holds one goal at a time. `GoalStarted` for a new task
//!   replaces the existing goal (and resets the per-goal objective
//!   text) but does NOT reset the session-scoped streak counter on
//!   [`GoalRuntime::continuation`] — that lives on the *session*, not
//!   the *task*, so a task that restarts via the input queue inherits
//!   the previous task's no-write streak (codex parity).
//! - **Streak is monotonic within a no-write run**: every
//!   `TurnCompleted { had_write = false }` increments
//!   [`ContinuationState::consecutive_no_write`]; the first
//!   `had_write = true` resets it to zero.
//! - **Continuation is enqueued exactly once per decision**: when
//!   [`Self::handle_event`] returns
//!   [`TaskRestart::Continuation`], the caller is responsible for
//!   pushing the carried [`UserInput`] onto the session's
//!   [`InputQueue`] *exactly once*. The runtime increments
//!   `total_continuation_turns` *before* returning, so a double-push
//!   from a buggy caller would over-count but never panic.
//! - **No direct `state.messages` mutation**: continuation prompts
//!   flow through [`InputQueue::push`] (via
//!   [`UserInput::Steer`]), routed through the same
//!   user-input pathway as a typed user message. The legacy
//!   `helpers::append_warning` path is no longer the injection
//!   surface.
//! - **`tokio::sync::Mutex` for state held across `.await`** (Rule
//!   6.1): both [`GoalRuntime::active_goal`] and
//!   [`GoalRuntime::continuation`] are guarded by a tokio mutex
//!   because the lock is taken from an `async fn` and the critical
//!   section (rendering the continuation body) can in principle
//!   `.await` future tracing / template work.
//!
//! # Failure modes
//!
//! - **Cancellation mid-decision**: the caller (turn stop hook /
//!   session idle handler) is the cancellation observer; the runtime
//!   itself never holds a lock across a foreign `.await` so a cancel
//!   landing after `handle_event` returns leaves the streak counter
//!   in a consistent state regardless of whether the
//!   `input_queue.push` actually fired.
//! - **`InputQueue::push` returns `AgentError::InputQueueClosed`**:
//!   surfaced verbatim through `?`. The caller may treat closed
//!   queues as a clean shutdown signal; the runtime itself does not
//!   attempt to recover.
//! - **`max_continuation_turns` exceeded**: the runtime returns
//!   [`TaskRestart::Blocked`] instead of another `Continuation`. The
//!   caller marks `state.result.stalled = true` and seeds
//!   `state.result.llm_error` with the canonical `task_blocked: …`
//!   prefix so dashboards / log greps continue to work (Phase 1.B
//!   wire-up preserved).
//!
//! # Tracing (Rule 12)
//!
//! [`Self::handle_event`] is wrapped in a `goal_runtime` span with
//! structured `session_id`, `task_id`, and `streak` fields. The
//! rendered continuation body is logged by hash only.

use std::collections::HashSet;
use std::path::PathBuf;

use tokio::sync::Mutex;
use tracing::{instrument, warn};

use crate::AgentError;

use super::{SessionId, UserInput};

// Re-export `TaskId` here so external callers and tests do not have to
// reach into `crate::agent_loop::task` through a sealed module path.
// `TaskId` lives on `agent_loop` because that's where tasks are run; the
// goal runtime simply borrows the type to attribute decisions back to
// the task they came from.
pub(crate) use crate::agent_loop::TaskId;

/// Lifecycle event consumed by the [`GoalRuntime`].
///
/// E.4 wires the `TurnCompleted` arm — the other variants exist as
/// codex-shaped placeholders for follow-up work (true session-idle
/// continuation and explicit goal-start handshakes). They round-trip
/// through [`GoalRuntime::handle_event`] today without mutating state
/// so callers can grow into them incrementally without API churn.
#[allow(dead_code)] // SessionIdle / MaybeContinueIfIdle / TaskBlocked are placeholder arms wired in follow-up.
#[derive(Debug, Clone)]
pub(crate) enum GoalRuntimeEvent {
    /// A new goal started — currently emitted at task start so the
    /// runtime can attribute subsequent `TurnCompleted` events to the
    /// right task. Does NOT reset
    /// [`ContinuationState::consecutive_no_write`] (the streak is
    /// session-scoped, not goal-scoped).
    GoalStarted {
        /// Identifier of the task carrying this goal.
        task_id: TaskId,
        /// Free-form objective text (typically the initial user
        /// message). Stored verbatim for tracing / future
        /// continuation prompt enrichment.
        objective: String,
    },
    /// One turn ended; carries the diff used to decide whether the
    /// runtime should escalate (no write) or reset (had a write).
    TurnCompleted {
        /// Task whose turn just ended.
        task_id: TaskId,
        /// `true` when the turn produced a successful write tool
        /// (`write_file` / `edit_file` / `delete_file`). Sourced
        /// directly from [`crate::agent_loop::turn_diff::TurnDiff`].
        had_write: bool,
        /// Read paths observed this turn. Reserved for the
        /// blocker-signature audit (codex `goal_spec.rs:79-80`); E.4
        /// does not yet inspect the contents.
        read_paths: HashSet<PathBuf>,
    },
    /// Session became idle (no active turn, queue empty). Placeholder
    /// for the codex `goals.rs:386-388` analog — wired into a
    /// future follow-up that surfaces true session-idle events.
    SessionIdle,
    /// External nudge to re-check the idle condition. Same as
    /// [`Self::SessionIdle`] for E.4 purposes.
    MaybeContinueIfIdle,
    /// Informational signal that the goal runtime escalated to
    /// `task_blocked`. Emitted alongside the
    /// [`TaskRestart::Blocked`] return so consumers that prefer
    /// event-stream semantics over function-return semantics can
    /// react. E.4 itself does not consume this variant.
    TaskBlocked {
        /// Task whose continuation budget was exhausted.
        task_id: TaskId,
        /// Owning session.
        session_id: SessionId,
        /// Consecutive-no-write streak at the moment of escalation.
        streak: u32,
    },
}

/// Outcome of [`GoalRuntime::handle_event`] when the runtime decides
/// the task shell must act before continuing.
///
/// `None` from [`GoalRuntime::handle_event`] means "no follow-up
/// required" (write happened, runtime disabled, or queue closed
/// silently). A `Some(_)` value carries either a continuation prompt
/// to push, or a "stop and mark blocked" escalation.
#[allow(dead_code)] // task_id / kind are surfaced via Debug + read by tests.
#[derive(Debug)]
pub(crate) enum TaskRestart {
    /// Push `input` via the session's [`InputQueue`] and run another
    /// turn. The runtime has already incremented
    /// `total_continuation_turns` for this decision; callers must not
    /// re-increment.
    Continuation {
        /// Task this continuation belongs to.
        task_id: TaskId,
        /// Soft `Nudge` or escalated `Blocked` kind. Mirrors the
        /// pre-E.4 [`ContinuationKind`] taxonomy verbatim.
        kind: ContinuationKind,
        /// The wrapped continuation prompt ready to be queued.
        input: UserInput,
        /// Consecutive-no-write streak as observed at decision time.
        streak: u32,
    },
    /// Streak exceeded [`GoalRuntime::max_continuation_turns`]. The
    /// caller must mark the task as `task_blocked`
    /// (`state.result.stalled = true` + `llm_error =
    /// "task_blocked: …"`) and unwind the turn loop.
    Blocked {
        /// Task whose continuation budget was exhausted.
        task_id: TaskId,
        /// Owning session.
        session_id: SessionId,
        /// Consecutive-no-write streak at the moment of escalation.
        streak: u32,
    },
}

/// Soft / hard escalation taxonomy for continuation prompts.
///
/// Migrated verbatim from the pre-E.4
/// `crate::agent_loop::continuation::ContinuationKind` so the
/// rendering tests and the agent-side runtime continue to agree on
/// the message envelope shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationKind {
    /// First two consecutive no-write iterations. Soft push: "either
    /// emit your next edit now, or call `task_done` with
    /// `no_changes_needed`".
    Nudge,
    /// Third no-write iteration and beyond — codex's "blocked-after-3"
    /// audit threshold. The runtime escalates the prompt language but
    /// still allows the agent another swing before
    /// `max_continuation_turns` trips the [`TaskRestart::Blocked`]
    /// path.
    Blocked,
}

/// Session-scoped streak / total-continuation tracker.
///
/// Owns the canonical counters consulted by [`GoalRuntime`]. Migrated
/// out of the pre-E.4 in-loop continuation module so the streak
/// survives task restarts (a task restarting via the input queue does
/// not reset the streak — codex parity).
#[derive(Debug, Default)]
pub(crate) struct ContinuationState {
    /// Number of consecutive no-write iterations observed so far.
    /// Reset to zero the moment a write lands.
    pub(crate) consecutive_no_write: u32,
    /// Cumulative count of continuation prompts injected this run.
    /// The runtime escalates to [`TaskRestart::Blocked`] once this
    /// reaches [`GoalRuntime::max_continuation_turns`].
    pub(crate) total_continuation_turns: u32,
    /// Last (up to 3) read-path sets from no-write iterations.
    /// Reserved for the blocker-signature audit; kept for parity with
    /// the pre-E.4 [`crate::agent_loop::continuation::ContinuationState`]
    /// shape (and so tests that move over continue to compile).
    #[allow(dead_code)]
    pub(crate) recent_no_write_paths: Vec<HashSet<PathBuf>>,
}

impl ContinuationState {
    /// Process one turn end. Returns `None` when a write reset the
    /// streak (and clears state), `Some(Nudge)` for the first two
    /// consecutive no-write iterations, `Some(Blocked)` from the
    /// third onward. Caller is responsible for any
    /// `total_continuation_turns` ceiling check.
    pub(crate) fn on_iteration_end(
        &mut self,
        had_write: bool,
        read_paths: HashSet<PathBuf>,
    ) -> Option<ContinuationKind> {
        if had_write {
            self.consecutive_no_write = 0;
            self.recent_no_write_paths.clear();
            return None;
        }
        self.consecutive_no_write = self.consecutive_no_write.saturating_add(1);
        self.recent_no_write_paths.push(read_paths);
        while self.recent_no_write_paths.len() > 3 {
            self.recent_no_write_paths.remove(0);
        }
        if self.consecutive_no_write >= 3 {
            Some(ContinuationKind::Blocked)
        } else {
            Some(ContinuationKind::Nudge)
        }
    }
}

/// Active goal context. E.4 stores only the originating task id and a
/// human-readable objective; codex's richer goal-spec lives on this
/// type in the follow-up that wires `submit_plan` into the runtime.
#[allow(dead_code)] // task_id is held for future blocker_signature attribution.
#[derive(Debug, Clone)]
pub(crate) struct GoalState {
    /// Task carrying this goal.
    pub(crate) task_id: TaskId,
    /// Free-form objective text (typically the initial user
    /// message). Kept for tracing and the future continuation-prompt
    /// enrichment pass that will splice the objective back into the
    /// rendered envelope.
    #[allow(dead_code)]
    pub(crate) objective: String,
}

/// Out-of-loop continuation driver. Receives session lifecycle events
/// and decides whether to push the next continuation prompt into
/// [`InputQueue`] or escalate to [`TaskRestart::Blocked`].
///
/// Owned by [`super::Session`] and shared across the agent loop's
/// turn-stop hook (which calls [`Self::handle_event`] after every
/// turn) and a future session-idle handler (planned follow-up).
pub(crate) struct GoalRuntime {
    /// Owning session id. Threaded into every emitted tracing span
    /// and every [`TaskRestart::Blocked`] return so the surfacing
    /// surface (CLI / dashboards) can correlate the decision with
    /// the session that produced it (Rule 4.3).
    session_id: SessionId,
    /// Hard cap on the number of continuation prompts the runtime
    /// will inject before escalating to [`TaskRestart::Blocked`].
    /// Forwarded from
    /// [`crate::AgentLoopConfig::max_continuation_turns`]; default
    /// `6` in production, lowered to `3` by the
    /// `goal_runtime_blocked_after_three_consecutive_no_write_iterations`
    /// unit test.
    max_continuation_turns: u32,
    /// At most one active goal per session. `None` before the first
    /// `GoalStarted` arrives and between goal lifetimes.
    active_goal: Mutex<Option<GoalState>>,
    /// Session-scoped streak counter; survives task restarts.
    continuation: Mutex<ContinuationState>,
}

impl GoalRuntime {
    /// Construct a fresh runtime for `session_id`. `max_continuation_turns`
    /// mirrors the same field on [`crate::AgentLoopConfig`].
    pub(crate) fn new(session_id: SessionId, max_continuation_turns: u32) -> Self {
        Self {
            session_id,
            max_continuation_turns,
            active_goal: Mutex::new(None),
            continuation: Mutex::new(ContinuationState::default()),
        }
    }

    /// The session this runtime belongs to.
    #[allow(dead_code)] // Surfaced by tests + the future SessionIdle handler.
    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Snapshot of the current streak. Cheap (one mutex lock, no
    /// `.await` while the lock is held except for the mutex itself).
    /// Used by the agent loop to populate
    /// [`crate::agent_loop::LoopState::continuation`] so the dev-loop
    /// reasoning-effort policy keeps working without re-plumbing
    /// `compute_thinking_effort`.
    #[allow(dead_code)] // Surfaced by tests; the loop reads `snapshot()` instead for both counters.
    pub(crate) async fn current_streak(&self) -> u32 {
        self.continuation.lock().await.consecutive_no_write
    }

    /// Snapshot of `(consecutive_no_write, total_continuation_turns)`
    /// — convenient for tests that want both counters in one lock
    /// acquisition.
    pub(crate) async fn snapshot(&self) -> ContinuationSnapshot {
        let guard = self.continuation.lock().await;
        ContinuationSnapshot {
            consecutive_no_write: guard.consecutive_no_write,
            total_continuation_turns: guard.total_continuation_turns,
        }
    }

    /// Dispatch one lifecycle event. Returns `Some(_)` when the task
    /// shell must act before the next turn (push a continuation
    /// prompt, or mark `task_blocked`); `None` when no follow-up is
    /// required.
    ///
    /// # Errors
    ///
    /// Propagates [`AgentError`] from any `.await`ed helper. E.4 itself
    /// has no fallible inner step — the signature is `Result` so
    /// `maybe_start_continuation` can grow a fallible body (e.g.
    /// template rendering errors) without churning callers.
    #[instrument(
        name = "goal_runtime",
        skip_all,
        fields(
            session_id = %self.session_id,
            streak = tracing::field::Empty,
            total = tracing::field::Empty,
        ),
    )]
    pub(crate) async fn handle_event(
        &self,
        event: GoalRuntimeEvent,
    ) -> Result<Option<TaskRestart>, AgentError> {
        match event {
            GoalRuntimeEvent::GoalStarted { task_id, objective } => {
                let mut goal_guard = self.active_goal.lock().await;
                *goal_guard = Some(GoalState { task_id, objective });
                // Streak is session-scoped — do NOT reset on
                // GoalStarted. A task restart via the input queue
                // re-emits GoalStarted; resetting here would defeat
                // the "streak survives task restarts" invariant
                // (codex parity, E.4 spec).
                Ok(None)
            }
            GoalRuntimeEvent::TurnCompleted {
                task_id,
                had_write,
                read_paths,
            } => self.on_turn_completed(task_id, had_write, read_paths).await,
            GoalRuntimeEvent::SessionIdle | GoalRuntimeEvent::MaybeContinueIfIdle => {
                self.maybe_start_continuation().await
            }
            GoalRuntimeEvent::TaskBlocked { .. } => {
                // Informational — already emitted by us alongside a
                // `TaskRestart::Blocked` return. Re-handling it does
                // not change state.
                Ok(None)
            }
        }
    }

    /// Internal: handle a `TurnCompleted` event. Increments / resets
    /// the streak, decides whether to nudge / escalate / block, and
    /// renders the continuation body when a push is required.
    async fn on_turn_completed(
        &self,
        task_id: TaskId,
        had_write: bool,
        read_paths: HashSet<PathBuf>,
    ) -> Result<Option<TaskRestart>, AgentError> {
        let mut cont = self.continuation.lock().await;
        let kind = cont.on_iteration_end(had_write, read_paths);
        tracing::Span::current().record("streak", cont.consecutive_no_write);
        tracing::Span::current().record("total", cont.total_continuation_turns);
        match kind {
            None => Ok(None),
            Some(kind) => {
                let streak = cont.consecutive_no_write;
                if cont.total_continuation_turns >= self.max_continuation_turns {
                    warn!(
                        session_id = %self.session_id,
                        task_id = %task_id,
                        streak,
                        total_continuation_turns = cont.total_continuation_turns,
                        max_continuation_turns = self.max_continuation_turns,
                        "max_continuation_turns exceeded; escalating to TaskBlocked"
                    );
                    return Ok(Some(TaskRestart::Blocked {
                        task_id,
                        session_id: self.session_id,
                        streak,
                    }));
                }
                let body = render(kind, streak as usize, streak);
                cont.total_continuation_turns = cont.total_continuation_turns.saturating_add(1);
                Ok(Some(TaskRestart::Continuation {
                    task_id,
                    kind,
                    input: UserInput::Steer { instruction: body },
                    streak,
                }))
            }
        }
    }

    /// Idle-handler entry point. E.4 does not yet wire true session-
    /// idle events (the in-loop `TurnCompleted` path covers Phase
    /// 1.B's continuation needs end-to-end), so this method is a
    /// `Ok(None)` placeholder. The signature matches codex's
    /// `maybe_start_goal_continuation_turn` so the follow-up that
    /// wires SessionIdle / MaybeContinueIfIdle can grow into it
    /// without API churn.
    pub(crate) async fn maybe_start_continuation(&self) -> Result<Option<TaskRestart>, AgentError> {
        Ok(None)
    }
}

/// Cheap value type returned by [`GoalRuntime::snapshot`]. Avoids
/// exposing the mutex-guarded [`ContinuationState`] outside the
/// module.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContinuationSnapshot {
    /// Consecutive no-write iterations observed.
    pub(crate) consecutive_no_write: u32,
    /// Total continuation prompts injected this session.
    pub(crate) total_continuation_turns: u32,
}

/// Render the continuation envelope injected before the next sampling
/// request. Body shape matches the pre-E.4
/// `crate::agent_loop::continuation::render` verbatim so existing
/// snapshot / contains assertions continue to hold after the
/// migration.
pub(crate) fn render(kind: ContinuationKind, iter: usize, count: u32) -> String {
    match kind {
        ContinuationKind::Nudge => format!(
            "<harness_continuation kind=\"nudge\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. No write_file / edit_file / delete_file ran this turn. The dev loop is not yet complete.\n\
             \n\
             Either emit your next edit now, or call `task_done` with `no_changes_needed: true` and `notes` explaining why no change is needed. The task description may be stale - if the codebase contradicts the spec, trust the codebase.\n\
             </harness_continuation>"
        ),
        ContinuationKind::Blocked => format!(
            "<harness_continuation kind=\"blocked\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. Three consecutive iterations without a write. This is the codex-style \"blocked\" audit threshold.\n\
             \n\
             You must do one of the following on the next turn:\n\
             1. Emit your best-effort write_file / edit_file / delete_file against the existing codebase.\n\
             2. Call `task_done` with `no_changes_needed: true` and `notes` describing the contradiction between the task description and the codebase.\n\
             \n\
             If you continue reading without one of the above, the harness will fail the task with `task_blocked`.\n\
             </harness_continuation>"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::TaskId;
    use crate::session::input_queue::InputQueue;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn empty_read_paths() -> HashSet<PathBuf> {
        HashSet::new()
    }

    fn fresh_runtime(max_turns: u32) -> GoalRuntime {
        GoalRuntime::new(SessionId::new_v4(), max_turns)
    }

    // --- continuation-state migration parity tests -----------------

    #[tokio::test]
    async fn continuation_state_resets_on_write() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap();
        assert_eq!(runtime.current_streak().await, 1);
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: true,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap();
        assert_eq!(
            runtime.current_streak().await,
            0,
            "a write must reset the session-scoped streak"
        );
    }

    #[tokio::test]
    async fn continuation_kind_escalates_after_three_no_writes() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        let mut kinds = Vec::new();
        for _ in 0..3 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                })
                .await
                .unwrap()
                .expect("no-write must produce a TaskRestart");
            match restart {
                TaskRestart::Continuation { kind, .. } => kinds.push(kind),
                TaskRestart::Blocked { .. } => {
                    panic!("did not exceed max_continuation_turns yet")
                }
            }
        }
        assert_eq!(
            kinds,
            vec![
                ContinuationKind::Nudge,
                ContinuationKind::Nudge,
                ContinuationKind::Blocked,
            ]
        );
    }

    // --- mandatory E.4 tests ----------------------------------------

    /// `goal_runtime_blocked_after_three_consecutive_no_write_iterations`
    /// (E.4 mandatory): three turns with no write produce three
    /// continuation prompts; the fourth no-write iteration trips
    /// `max_continuation_turns` and the runtime returns
    /// `TaskRestart::Blocked`. Uses `tokio::test(start_paused = true)`
    /// per Rule 7.3 so any future time-based escalation stays
    /// deterministic.
    #[tokio::test(start_paused = true)]
    async fn goal_runtime_blocked_after_three_consecutive_no_write_iterations() {
        let session_id = SessionId::new_v4();
        let runtime = GoalRuntime::new(session_id, 3);
        let task = TaskId::new_v4();

        for _ in 0..3 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                })
                .await
                .unwrap()
                .expect("no-write iterations must yield Continuation");
            assert!(matches!(restart, TaskRestart::Continuation { .. }));
        }
        let blocked = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap()
            .expect("fourth no-write iteration must escalate to Blocked");
        match blocked {
            TaskRestart::Blocked {
                task_id,
                session_id: blocked_session,
                streak,
            } => {
                assert_eq!(task_id, task);
                assert_eq!(blocked_session, session_id);
                assert_eq!(
                    streak, 4,
                    "fourth no-write turn must report streak=4 at escalation"
                );
            }
            other => panic!("expected TaskRestart::Blocked, got {other:?}"),
        }
    }

    /// `goal_runtime_streak_survives_task_restart` (E.4 mandatory):
    /// a task restart (modelled here as a fresh `GoalStarted` for a
    /// new `TaskId` on the same `GoalRuntime`) does NOT reset the
    /// streak. The session-scoped tracker continues to escalate the
    /// next no-write iteration as if no restart had happened.
    #[tokio::test(start_paused = true)]
    async fn goal_runtime_streak_survives_task_restart() {
        let runtime = fresh_runtime(6);
        let task_a = TaskId::new_v4();
        // Two no-writes on task A push the streak to 2.
        for _ in 0..2 {
            runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task_a,
                    had_write: false,
                    read_paths: empty_read_paths(),
                })
                .await
                .unwrap();
        }
        assert_eq!(runtime.current_streak().await, 2);

        // Task B starts (e.g. input queue fired). Streak must NOT
        // reset.
        let task_b = TaskId::new_v4();
        runtime
            .handle_event(GoalRuntimeEvent::GoalStarted {
                task_id: task_b,
                objective: "follow-up".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            runtime.current_streak().await,
            2,
            "GoalStarted for a fresh task must not reset the session-scoped streak"
        );

        // The very next no-write on task B escalates the streak to 3
        // and produces a Blocked continuation kind (the kind, not
        // the TaskRestart::Blocked escalation — that needs
        // `total >= max_continuation_turns`).
        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task_b,
                had_write: false,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap()
            .expect("third no-write must escalate kind");
        match restart {
            TaskRestart::Continuation { kind, streak, .. } => {
                assert_eq!(kind, ContinuationKind::Blocked);
                assert_eq!(streak, 3);
            }
            other => panic!("expected Continuation, got {other:?}"),
        }
    }

    /// `goal_runtime_routes_continuation_through_input_queue` (E.4
    /// mandatory): the runtime returns a `TaskRestart::Continuation`
    /// whose `input` arrives via `UserInput::Steer`. Pushing that
    /// input through a real [`InputQueue`] must surface it on the
    /// queue (not via direct `state.messages` mutation — there is no
    /// `state` at this seam at all). The test consumes the queue
    /// and asserts the variant + envelope tag.
    #[tokio::test(start_paused = true)]
    async fn goal_runtime_routes_continuation_through_input_queue() {
        let session_id = SessionId::new_v4();
        let cancel = CancellationToken::new();
        let queue = InputQueue::new(session_id, cancel.clone());
        let runtime = GoalRuntime::new(session_id, 6);
        let task = TaskId::new_v4();

        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap()
            .expect("first no-write must produce a Continuation");

        let input = match restart {
            TaskRestart::Continuation { input, .. } => input,
            other => panic!("expected Continuation, got {other:?}"),
        };
        match &input {
            UserInput::Steer { instruction } => {
                assert!(
                    instruction.contains("<harness_continuation"),
                    "rendered body must carry the harness_continuation envelope"
                );
            }
            other => panic!("expected UserInput::Steer, got {other:?}"),
        }

        // Push it through the queue exactly as the turn stop hook
        // would. The drain side then sees the same payload — proves
        // the continuation flows through the user-input boundary
        // rather than via a direct message-list mutation.
        queue.push(input).await.unwrap();
        let drained = queue.drain().await;
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            UserInput::Steer { instruction } => {
                assert!(instruction.contains("<harness_continuation"));
            }
            other => panic!("expected Steer on queue, got {other:?}"),
        }
        // Cancellation token is borrowed by the queue's internals;
        // hold a reference so it stays alive for the test.
        drop(cancel);
    }

    #[tokio::test]
    async fn maybe_start_continuation_is_a_noop_placeholder() {
        let runtime = fresh_runtime(6);
        let outcome = runtime.maybe_start_continuation().await.unwrap();
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn render_nudge_includes_envelope_and_iteration() {
        let body = render(ContinuationKind::Nudge, 4, 1);
        assert!(body.contains("<harness_continuation kind=\"nudge\""));
        assert!(body.contains("iteration=\"4\""));
        assert!(body.contains("consecutive_no_write=\"1\""));
        assert!(body.contains("trust the codebase"));
        assert!(body.contains("</harness_continuation>"));
    }

    #[tokio::test]
    async fn render_blocked_includes_envelope_and_options() {
        let body = render(ContinuationKind::Blocked, 7, 3);
        assert!(body.contains("<harness_continuation kind=\"blocked\""));
        assert!(body.contains("iteration=\"7\""));
        assert!(body.contains("consecutive_no_write=\"3\""));
        assert!(body.contains("task_blocked"));
        assert!(body.contains("</harness_continuation>"));
    }

    #[tokio::test]
    async fn snapshot_returns_both_counters() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
            })
            .await
            .unwrap();
        let snap = runtime.snapshot().await;
        assert_eq!(snap.consecutive_no_write, 1);
        assert_eq!(snap.total_continuation_turns, 1);
    }
}
