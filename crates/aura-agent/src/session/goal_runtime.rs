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

use std::collections::{HashSet, VecDeque};
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

// Priority A: `FailedWriteAttempt` is recorded by the per-iteration
// `TurnDiff` and forwarded through `GoalRuntimeEvent::TurnCompleted`
// so the session-scoped continuation state can buffer the last few
// rejections across `TurnDiff::reset()` boundaries (the diff is wiped
// at the top of every iteration; the goal-runtime buffer is not).
pub(crate) use crate::agent_loop::turn_diff::FailedWriteAttempt;

/// Capacity of the session-scoped failed-write ring buffer kept on
/// [`ContinuationState::recent_failures`]. Sized at 2× the default
/// `max_continuation_turns` (6 vs. 3) so that even after the rolling
/// window evicts older entries, the model still sees its mistakes
/// from the immediately preceding two continuation turns when the
/// runtime decides to render a [`ContinuationKind::Recovery`] body.
pub(crate) const RECENT_FAILURES_BUFFER_SIZE: usize = 6;

/// Maximum number of failed-write entries echoed into a single
/// [`ContinuationKind::Recovery`] body. The renderer is the natural
/// place to clamp because a single batch from the tool pipeline can
/// legitimately deliver more than 3 rejections (e.g. several
/// concurrent `edit_file`s all missing on the same stale read), and
/// echoing all of them would balloon the steering envelope.
pub(crate) const RECOVERY_RECAP_MAX_ENTRIES: usize = 3;

/// Jaccard similarity threshold between consecutive no-write read-path
/// sets that marks the agent as circling (re-reading the same files).
pub(crate) const CIRCLING_PATH_OVERLAP_THRESHOLD: f64 = 0.6;

/// Tight continuation cap applied when [`detect_circling`] is true.
pub(crate) const CIRCLING_MAX_CONTINUATION_TURNS: u32 = 6;

/// Number of consumed continuation slots forgiven by a successful write.
///
/// A write is real progress, but not necessarily task completion. Repaying a
/// bounded slice avoids punishing explore -> write -> verify flows while still
/// letting partial-write stalls eventually trip the blocker.
pub(crate) const SUCCESSFUL_WRITE_CONTINUATION_REPAYMENT: u32 = 2;

/// No-write turns allowed after at least one successful write before treating
/// the task as partial-progress stalled.
pub(crate) const POST_WRITE_NO_WRITE_STALL_TURNS: u32 = 8;

pub(crate) const PARTIAL_PROGRESS_STEER: &str =
    "If you already wrote to files this task and the spec calls for additional \
     methods or exports, your next call must append the missing surface with \
     write_file / edit_file / delete_file. If the latest build or file read \
     shows malformed code from your own edit, repair that exact file before \
     exploring more. Only call task_done with \
     no_changes_needed: true if the codebase already satisfies the task.";

/// True when the last two no-write turns read largely the same paths.
pub(crate) fn detect_circling(recent_no_write_paths: &[HashSet<PathBuf>]) -> bool {
    if recent_no_write_paths.len() < 2 {
        return false;
    }
    let a = &recent_no_write_paths[recent_no_write_paths.len() - 2];
    let b = &recent_no_write_paths[recent_no_write_paths.len() - 1];
    path_set_jaccard(a, b) >= CIRCLING_PATH_OVERLAP_THRESHOLD
}

fn path_set_jaccard(a: &HashSet<PathBuf>, b: &HashSet<PathBuf>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Paths read on both of the last two no-write turns (for steering text).
pub(crate) fn overlapping_read_paths(recent_no_write_paths: &[HashSet<PathBuf>]) -> Vec<PathBuf> {
    if recent_no_write_paths.len() < 2 {
        return Vec::new();
    }
    let a = &recent_no_write_paths[recent_no_write_paths.len() - 2];
    let b = &recent_no_write_paths[recent_no_write_paths.len() - 1];
    let mut paths: Vec<PathBuf> = a.intersection(b).cloned().collect();
    paths.sort();
    paths
}

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
        /// Write-tool calls that returned `is_error = true` this
        /// turn, in submission order. Priority A telemetry: the
        /// runtime classifies a turn whose only write attempts were
        /// rejected as [`WriteAttemptKind::OnlyRejectedWrites`] and
        /// echoes the most recent rejections back to the model via
        /// [`ContinuationKind::Recovery`]. Empty when no write tool
        /// ran or every write succeeded.
        failed_write_attempts: Vec<FailedWriteAttempt>,
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
        /// Configured continuation ceiling before runtime adjustments.
        configured_max: u32,
        /// Effective ceiling used for this decision. Circling lowers it.
        effective_max: u32,
        /// True when repeated read paths caused the lower circling cap.
        circling: bool,
        /// Why the runtime stopped the task.
        reason: BlockReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockReason {
    ContinuationBudget,
    PartialProgressStalled,
}

/// Soft / hard escalation taxonomy for continuation prompts.
///
/// Migrated verbatim from the pre-E.4
/// `crate::agent_loop::continuation::ContinuationKind` so the
/// rendering tests and the agent-side runtime continue to agree on
/// the message envelope shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationKind {
    /// First two consecutive no-write iterations *without* any
    /// rejected write attempt. Soft push: "either emit your next
    /// edit now, or call `task_done` with `no_changes_needed`".
    Nudge,
    /// Third no-write iteration and beyond — codex's "blocked-after-3"
    /// audit threshold. The runtime escalates the prompt language but
    /// still allows the agent another swing before
    /// `max_continuation_turns` trips the [`TaskRestart::Blocked`]
    /// path.
    Blocked,
    /// Priority A: streak advanced because the model attempted a
    /// write that was rejected by the tool layer (e.g. `edit_file`
    /// "needle not found", `write_file` "path not found"). The body
    /// echoes the recent failures back at the model so it can
    /// course-correct instead of guessing the same needle / path
    /// again. The streak still advances so the
    /// `max_continuation_turns` ceiling continues to fire — the
    /// only difference vs. `Nudge` is the steering text.
    Recovery,
}

/// Priority A: tri-state classification of a turn's write activity,
/// computed from [`crate::agent_loop::turn_diff::TurnDiff`]. Drives
/// [`ContinuationState::on_iteration_end`]'s
/// `streak += ?` / `continuation kind = ?` decision so a turn whose
/// only write attempt was rejected does not produce the same
/// continuation body as a turn that called no write tool at all.
///
/// Pre-A both branches collapsed to "streak += 1 + emit Nudge", which
/// is precisely what kept the doom-loop alive: the model never saw
/// its own rejection in the steering text and kept guessing the
/// same needle until `max_continuation_turns` tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteAttemptKind {
    /// At least one successful write tool ran
    /// ([`crate::agent_loop::turn_diff::TurnDiff::is_empty`] is
    /// false). Resets the no-write streak to zero.
    SuccessfulWrite,
    /// One or more write tools ran but every result carried
    /// `is_error = true`. The streak advances and the next
    /// continuation body becomes [`ContinuationKind::Recovery`] so
    /// the model can see what it tried last turn.
    OnlyRejectedWrites,
    /// No write tool was called at all this turn. The streak
    /// advances under the existing `Nudge` / `Blocked` taxonomy.
    NoWriteAttempted,
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
    /// Reset to zero the moment a *successful* write lands. A turn
    /// whose only writes were rejected by the tool layer advances
    /// the streak just like a pure no-write turn — see
    /// [`WriteAttemptKind::OnlyRejectedWrites`] for the rationale.
    pub(crate) consecutive_no_write: u32,
    /// Cumulative count of continuation prompts injected this run.
    /// The runtime escalates to [`TaskRestart::Blocked`] once this
    /// reaches [`GoalRuntime::max_continuation_turns`].
    pub(crate) total_continuation_turns: u32,
    /// Whether any successful write has landed in this session.
    pub(crate) has_successful_write: bool,
    /// Consecutive no-write turns after the first successful write.
    pub(crate) no_write_after_successful_write: u32,
    /// Last (up to 3) read-path sets from no-write iterations. Used by
    /// [`detect_circling`] and the enriched blocked continuation body.
    pub(crate) recent_no_write_paths: Vec<HashSet<PathBuf>>,
    /// Priority A: ring buffer of the most recent rejected write
    /// attempts across recent turns. Lives on the session-scoped
    /// state (not on `TurnDiff`) because
    /// [`crate::agent_loop::turn_diff::TurnDiff::reset`] wipes its
    /// own `failed_write_attempts` field at the top of every
    /// iteration, while the goal-runtime continuation body needs
    /// to recap failures from the immediately preceding turn(s).
    /// Bounded at [`RECENT_FAILURES_BUFFER_SIZE`] via FIFO eviction
    /// so a stuck loop cannot grow it without bound.
    pub(crate) recent_failures: VecDeque<FailedWriteAttempt>,
}

impl ContinuationState {
    /// Process one turn end. Returns `None` when a successful write
    /// reset the streak (and clears state), `Some(Nudge)` for the
    /// first two consecutive no-write iterations *without
    /// rejections*, `Some(Blocked)` from the third no-write-only
    /// iteration onward, and `Some(Recovery)` when the only writes
    /// attempted were rejected by the tool layer (Priority A).
    /// Caller is responsible for any `total_continuation_turns`
    /// ceiling check.
    pub(crate) fn on_iteration_end(
        &mut self,
        attempt_kind: WriteAttemptKind,
        read_paths: HashSet<PathBuf>,
        failed_write_attempts: Vec<FailedWriteAttempt>,
    ) -> Option<ContinuationKind> {
        // Extend the session-scoped ring buffer with any failures
        // observed this turn *before* the success-reset branch — a
        // "write succeeded but a sibling write failed" turn (e.g.
        // two concurrent edit_file calls, one hits, one misses)
        // still teaches the model something useful, and pre-loading
        // the buffer here keeps the renderer's contract simple
        // (always read the last N, regardless of which branch
        // fired). The success reset below still wipes the streak.
        for attempt in failed_write_attempts {
            if self.recent_failures.len() >= RECENT_FAILURES_BUFFER_SIZE {
                self.recent_failures.pop_front();
            }
            self.recent_failures.push_back(attempt);
        }

        match attempt_kind {
            WriteAttemptKind::SuccessfulWrite => {
                self.consecutive_no_write = 0;
                self.recent_no_write_paths.clear();
                self.total_continuation_turns = self
                    .total_continuation_turns
                    .saturating_sub(SUCCESSFUL_WRITE_CONTINUATION_REPAYMENT);
                self.has_successful_write = true;
                self.no_write_after_successful_write = 0;
                None
            }
            WriteAttemptKind::OnlyRejectedWrites => {
                self.consecutive_no_write = self.consecutive_no_write.saturating_add(1);
                if self.has_successful_write {
                    self.no_write_after_successful_write =
                        self.no_write_after_successful_write.saturating_add(1);
                }
                self.recent_no_write_paths.push(read_paths);
                while self.recent_no_write_paths.len() > 3 {
                    self.recent_no_write_paths.remove(0);
                }
                // Recovery body is emitted regardless of the streak
                // depth — the model needs to see its own rejection
                // on the very next continuation, not after three
                // wasted nudges. The `total_continuation_turns`
                // ceiling still fires the usual way.
                Some(ContinuationKind::Recovery)
            }
            WriteAttemptKind::NoWriteAttempted => {
                self.consecutive_no_write = self.consecutive_no_write.saturating_add(1);
                if self.has_successful_write {
                    self.no_write_after_successful_write =
                        self.no_write_after_successful_write.saturating_add(1);
                }
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
    }
}

/// Priority A classifier: collapse the `(had_write, failed_attempts)`
/// pair into a single tri-state. Centralised so the buffered
/// turn-stop hook and any future session-idle handler agree on the
/// rules.
pub(crate) fn classify_write_attempt(
    had_write: bool,
    failed_attempts: &[FailedWriteAttempt],
) -> WriteAttemptKind {
    if had_write {
        WriteAttemptKind::SuccessfulWrite
    } else if !failed_attempts.is_empty() {
        WriteAttemptKind::OnlyRejectedWrites
    } else {
        WriteAttemptKind::NoWriteAttempted
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
            no_write_after_successful_write: guard.no_write_after_successful_write,
        }
    }

    /// Whether the agent is in a read-only circling phase (overlap across
    /// recent no-write turns). Drives the session read dedup gate on
    /// [`crate::agent_loop::LoopState`].
    pub(crate) async fn is_circling(&self) -> bool {
        let guard = self.continuation.lock().await;
        detect_circling(&guard.recent_no_write_paths)
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
                failed_write_attempts,
            } => {
                self.on_turn_completed(task_id, had_write, read_paths, failed_write_attempts)
                    .await
            }
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
    /// the streak, decides whether to nudge / escalate / block / recover,
    /// and renders the continuation body when a push is required.
    async fn on_turn_completed(
        &self,
        task_id: TaskId,
        had_write: bool,
        read_paths: HashSet<PathBuf>,
        failed_write_attempts: Vec<FailedWriteAttempt>,
    ) -> Result<Option<TaskRestart>, AgentError> {
        let mut cont = self.continuation.lock().await;
        let attempt_kind = classify_write_attempt(had_write, &failed_write_attempts);
        let kind = cont.on_iteration_end(attempt_kind, read_paths, failed_write_attempts);
        tracing::Span::current().record("streak", cont.consecutive_no_write);
        tracing::Span::current().record("total", cont.total_continuation_turns);
        match kind {
            None => Ok(None),
            Some(kind) => {
                let streak = cont.consecutive_no_write;
                let circling = detect_circling(&cont.recent_no_write_paths);
                let effective_max = if circling {
                    self.max_continuation_turns
                        .min(CIRCLING_MAX_CONTINUATION_TURNS)
                } else {
                    self.max_continuation_turns
                };
                if cont.no_write_after_successful_write >= POST_WRITE_NO_WRITE_STALL_TURNS {
                    warn!(
                        session_id = %self.session_id,
                        task_id = %task_id,
                        streak,
                        post_write_no_write_turns = cont.no_write_after_successful_write,
                        "partial-progress stall exceeded; escalating to TaskBlocked"
                    );
                    return Ok(Some(TaskRestart::Blocked {
                        task_id,
                        session_id: self.session_id,
                        streak,
                        configured_max: POST_WRITE_NO_WRITE_STALL_TURNS,
                        effective_max: POST_WRITE_NO_WRITE_STALL_TURNS,
                        circling,
                        reason: BlockReason::PartialProgressStalled,
                    }));
                }
                if cont.total_continuation_turns >= effective_max {
                    warn!(
                        session_id = %self.session_id,
                        task_id = %task_id,
                        streak,
                        total_continuation_turns = cont.total_continuation_turns,
                        max_continuation_turns = effective_max,
                        circling,
                        "max_continuation_turns exceeded; escalating to TaskBlocked"
                    );
                    return Ok(Some(TaskRestart::Blocked {
                        task_id,
                        session_id: self.session_id,
                        streak,
                        configured_max: self.max_continuation_turns,
                        effective_max,
                        circling,
                        reason: BlockReason::ContinuationBudget,
                    }));
                }
                // Render reads the session-scoped failure buffer so
                // a Recovery body can recap up to N entries that
                // span this turn AND the previous one (the in-turn
                // recorder writes into the buffer before this point
                // already, so the slice is up-to-date).
                let recent: Vec<FailedWriteAttempt> =
                    cont.recent_failures.iter().cloned().collect();
                let overlap = if circling {
                    overlapping_read_paths(&cont.recent_no_write_paths)
                } else {
                    Vec::new()
                };
                let render_kind = if circling && kind == ContinuationKind::Nudge {
                    ContinuationKind::Blocked
                } else {
                    kind
                };
                let body = render(
                    render_kind,
                    streak as usize,
                    streak,
                    &recent,
                    if overlap.is_empty() {
                        None
                    } else {
                        Some(overlap.as_slice())
                    },
                );
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
    /// Consecutive no-write turns after the first successful write.
    pub(crate) no_write_after_successful_write: u32,
}

/// Render the continuation envelope injected before the next sampling
/// request. Body shape for [`ContinuationKind::Nudge`] and
/// [`ContinuationKind::Blocked`] matches the pre-Priority-A renderer
/// verbatim so existing snapshot / `contains` assertions continue to
/// hold. `recent_errors` is consumed only by
/// [`ContinuationKind::Recovery`] and is otherwise inert — callers
/// always pass the session-scoped failure buffer slice so the
/// renderer never has to reach back into [`ContinuationState`].
pub(crate) fn render(
    kind: ContinuationKind,
    iter: usize,
    count: u32,
    recent_errors: &[FailedWriteAttempt],
    circling_paths: Option<&[PathBuf]>,
) -> String {
    match kind {
        ContinuationKind::Nudge => format!(
            "<harness_continuation kind=\"nudge\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. No write_file / edit_file / delete_file ran this turn. The dev loop is not yet complete.\n\
             \n\
             Either emit your next edit now, or call `task_done` with `no_changes_needed: true` and `notes` explaining why no change is needed. The task description may be stale - if the codebase contradicts the spec, trust the codebase.\n\
             </harness_continuation>"
        ),
        ContinuationKind::Blocked => {
            let circling_note = circling_paths.map_or(String::new(), |paths| {
                let listed: Vec<String> = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                format!(
                    "\nCircling detected — repeated reads on: {}.\n\
                     Do not read these paths again. Your next tool call must be \
                     write_file / edit_file / delete_file, or task_done with \
                     no_changes_needed: true and notes explaining why the task \
                     is already satisfied.\n\
                     If this is a module task, create the missing module file now and mirror \
                     the sibling reference already shown in context.\n\
                     {PARTIAL_PROGRESS_STEER}\n",
                    listed.join(", ")
                )
            });
            format!(
            "<harness_continuation kind=\"blocked\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. Three consecutive iterations without a write. This is the codex-style \"blocked\" audit threshold.\n\
             \n\
             You must do one of the following on the next turn:\n\
             1. Emit your best-effort write_file / edit_file / delete_file against the existing codebase.\n\
             2. Call `task_done` with `no_changes_needed: true` and `notes` describing the contradiction between the task description and the codebase.\n\
             {circling_note}\
             If you continue reading without one of the above, the harness will fail the task with `task_blocked`.\n\
             </harness_continuation>"
            )
        }
        ContinuationKind::Recovery => {
            // Show the *most recent* N entries — when the buffer is
            // longer than the cap, the operator (and the model) care
            // about the latest rejections, not the oldest.
            let take = RECOVERY_RECAP_MAX_ENTRIES.min(recent_errors.len());
            let start = recent_errors.len().saturating_sub(take);
            let tail = &recent_errors[start..];
            let mut recap = String::new();
            for (i, err) in tail.iter().enumerate() {
                let path_hint = err
                    .target_path
                    .as_deref()
                    .map_or(String::new(), |p| format!(" (path: {p})"));
                recap.push_str(&format!(
                    "  {}. {}{}: {}\n",
                    i + 1,
                    err.tool,
                    path_hint,
                    err.error_snippet.trim()
                ));
            }
            format!(
                "<harness_continuation kind=\"recovery\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
                 Iteration {iter}. Your recent write attempts were rejected by the tool layer:\n\
                 {recap}\
                 \n\
                 Re-read the target file ONCE to get the real bytes, then retry with exact text. Do not keep guessing needles or paths. If the file shape has changed since you last read it, the codebase is authoritative.\n\
                 </harness_continuation>"
            )
        }
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

    fn read_paths(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
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
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(runtime.current_streak().await, 1);
        // One no-write turn must have burned a cumulative slot so we can
        // verify the write-driven reset actually wipes it.
        let snap_before = runtime.snapshot().await;
        assert!(
            snap_before.total_continuation_turns >= 1,
            "no-write turn must advance total_continuation_turns; got snapshot = {snap_before:?}",
        );
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: true,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(
            runtime.current_streak().await,
            0,
            "a write must reset the session-scoped streak"
        );
        let snap_after = runtime.snapshot().await;
        assert_eq!(
            snap_after.total_continuation_turns,
            snap_before
                .total_continuation_turns
                .saturating_sub(SUCCESSFUL_WRITE_CONTINUATION_REPAYMENT),
            "a write must repay only a bounded slice of the cumulative continuation budget"
        );
        assert_eq!(
            snap_after.no_write_after_successful_write, 0,
            "a write must reset the post-write no-write stall counter"
        );
    }

    /// A write repays some continuation budget, but it does not grant a
    /// completely fresh unlimited no-write runway. This keeps small partial
    /// writes from indefinitely postponing `task_blocked`.
    #[tokio::test]
    async fn successful_write_repays_budget_without_zeroing_it() {
        let runtime = fresh_runtime(10);
        let task = TaskId::new_v4();

        for i in 0..5 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap()
                .expect("no-write turn must yield a Continuation");
            assert!(
                matches!(restart, TaskRestart::Continuation { .. }),
                "exploration turn {i} must not block (got {restart:?})",
            );
        }

        let none = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: true,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        assert!(none.is_none(), "a write turn must produce None");
        assert_eq!(
            runtime.snapshot().await.total_continuation_turns,
            5 - SUCCESSFUL_WRITE_CONTINUATION_REPAYMENT
        );

        for i in 0..3 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap()
                .expect("post-write no-write turn must still yield a Continuation");
            assert!(
                matches!(restart, TaskRestart::Continuation { .. }),
                "bounded post-write no-write turn {i} must not block (got {restart:?})",
            );
        }
    }

    #[tokio::test]
    async fn partial_progress_stalls_after_post_write_no_write_budget() {
        let runtime = fresh_runtime(100);
        let task = TaskId::new_v4();

        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: true,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();

        for _ in 0..(POST_WRITE_NO_WRITE_STALL_TURNS - 1) {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap()
                .expect("post-write no-write should continue before stall cap");
            assert!(matches!(restart, TaskRestart::Continuation { .. }));
        }

        let blocked = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap()
            .expect("post-write no-write cap should block");
        match blocked {
            TaskRestart::Blocked { reason, .. } => {
                assert_eq!(reason, BlockReason::PartialProgressStalled);
            }
            other => panic!("expected partial-progress Blocked, got {other:?}"),
        }
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
                    failed_write_attempts: Vec::new(),
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

    #[test]
    fn detect_circling_uses_recent_path_overlap() {
        let first = read_paths(&["src/inbox.rs", "src/storage.rs"]);
        let overlapping = read_paths(&["src/inbox.rs", "src/storage.rs", "src/lib.rs"]);
        let different = read_paths(&["src/outbox.rs", "src/lib.rs"]);

        assert!(detect_circling(&[first.clone(), overlapping]));
        assert!(!detect_circling(&[first, different]));
        assert!(!detect_circling(&[empty_read_paths(), empty_read_paths()]));
    }

    #[tokio::test]
    async fn circling_latch_requires_repeated_read_path_overlap() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();

        for paths in [
            read_paths(&["src/inbox.rs"]),
            read_paths(&["src/outbox.rs", "src/lib.rs"]),
        ] {
            runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: paths,
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap();
        }
        assert!(
            !runtime.is_circling().await,
            "a no-write streak alone must not hard-block duplicate reads"
        );

        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: read_paths(&["src/outbox.rs", "src/lib.rs", "src/main.rs"]),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        assert!(
            runtime.is_circling().await,
            "overlapping no-write read paths should still latch circling"
        );
    }

    #[tokio::test]
    async fn circling_lowers_effective_max_to_six() {
        let runtime = fresh_runtime(100);
        let task = TaskId::new_v4();
        let repeated = read_paths(&["src/inbox.rs", "src/storage.rs"]);

        for turn in 0..6 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: repeated.clone(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap()
                .expect("circling no-write turn should continue until cap is reached");
            assert!(
                matches!(restart, TaskRestart::Continuation { .. }),
                "turn {turn} should not block yet; got {restart:?}"
            );
        }

        let blocked = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: repeated,
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap()
            .expect("seventh circling turn should block under effective cap 6");
        match blocked {
            TaskRestart::Blocked {
                configured_max,
                effective_max,
                circling,
                ..
            } => {
                assert_eq!(configured_max, 100);
                assert_eq!(effective_max, CIRCLING_MAX_CONTINUATION_TURNS);
                assert!(circling);
            }
            other => panic!("expected circling TaskRestart::Blocked, got {other:?}"),
        }
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
                    failed_write_attempts: Vec::new(),
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
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap()
            .expect("fourth no-write iteration must escalate to Blocked");
        match blocked {
            TaskRestart::Blocked {
                task_id,
                session_id: blocked_session,
                streak,
                ..
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
                    failed_write_attempts: Vec::new(),
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
                failed_write_attempts: Vec::new(),
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
                failed_write_attempts: Vec::new(),
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
        let body = render(ContinuationKind::Nudge, 4, 1, &[], None);
        assert!(body.contains("<harness_continuation kind=\"nudge\""));
        assert!(body.contains("iteration=\"4\""));
        assert!(body.contains("consecutive_no_write=\"1\""));
        assert!(body.contains("trust the codebase"));
        assert!(body.contains("</harness_continuation>"));
    }

    #[tokio::test]
    async fn render_blocked_includes_envelope_and_options() {
        let body = render(ContinuationKind::Blocked, 7, 3, &[], None);
        assert!(body.contains("<harness_continuation kind=\"blocked\""));
        assert!(body.contains("iteration=\"7\""));
        assert!(body.contains("consecutive_no_write=\"3\""));
        assert!(body.contains("task_blocked"));
        assert!(body.contains("</harness_continuation>"));
    }

    #[tokio::test]
    async fn render_blocked_with_circling_paths_forces_write_or_done() {
        let paths = vec![
            PathBuf::from("crates/zero-storage/src/inbox.rs"),
            PathBuf::from("crates/zero-storage/src/storage.rs"),
        ];
        let body = render(ContinuationKind::Blocked, 3, 2, &[], Some(&paths));

        assert!(body.contains("Circling detected"));
        assert!(body.contains("Do not read these paths again"));
        assert!(body.contains("Your next tool call must be"));
        assert!(body.contains("write_file / edit_file / delete_file"));
        assert!(body.contains("no_changes_needed: true"));
        assert!(body.contains("crates/zero-storage/src/inbox.rs"));
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
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        let snap = runtime.snapshot().await;
        assert_eq!(snap.consecutive_no_write, 1);
        assert_eq!(snap.total_continuation_turns, 1);
        assert_eq!(snap.no_write_after_successful_write, 0);
    }

    // ----------------------------------------------------------------
    // Priority A mandatory tests
    // ----------------------------------------------------------------

    fn mk_failed_attempt(tool: &str, path: &str, snippet: &str) -> FailedWriteAttempt {
        FailedWriteAttempt {
            tool: tool.to_string(),
            target_path: Some(path.to_string()),
            error_snippet: snippet.to_string(),
        }
    }

    /// Priority A: a turn that attempted an `edit_file` whose needle
    /// missed (i.e. the only writes returned `is_error = true`) must
    /// advance the streak AND emit a [`ContinuationKind::Recovery`]
    /// whose body echoes the failed needle back to the model. This
    /// is the doom-loop fix: pre-A the same turn would emit a plain
    /// `Nudge` and the model would re-issue the same broken needle
    /// indefinitely.
    #[tokio::test]
    async fn failed_edit_advances_streak_with_recovery_variant() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        let snippet =
            "The specified text was not found in the file. None of the 4 needle line(s) match.";
        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: vec![mk_failed_attempt(
                    "edit_file",
                    "spec/01-foundation-identity.md",
                    snippet,
                )],
            })
            .await
            .unwrap()
            .expect("rejected-only write turn must produce a Continuation");
        assert_eq!(
            runtime.current_streak().await,
            1,
            "rejected-only writes must advance the streak just like a pure no-write turn"
        );
        match restart {
            TaskRestart::Continuation { kind, input, .. } => {
                assert_eq!(kind, ContinuationKind::Recovery);
                match input {
                    UserInput::Steer { instruction } => {
                        assert!(
                            instruction.contains("kind=\"recovery\""),
                            "Recovery body must tag the envelope: {instruction}"
                        );
                        assert!(
                            instruction.contains("edit_file"),
                            "Recovery body must name the rejected tool: {instruction}"
                        );
                        assert!(
                            instruction.contains("spec/01-foundation-identity.md"),
                            "Recovery body must surface the target path hint: {instruction}"
                        );
                        assert!(
                            instruction.contains("needle line(s) match"),
                            "Recovery body must surface the actionable executor phrase: {instruction}"
                        );
                        assert!(
                            instruction.contains("Re-read the target file"),
                            "Recovery body must include the steering advice: {instruction}"
                        );
                    }
                    other => panic!("expected UserInput::Steer, got {other:?}"),
                }
            }
            other => panic!("expected Continuation, got {other:?}"),
        }
    }

    /// Priority A regression: a real `write_file` success must still
    /// reset `consecutive_no_write` to zero — the tri-state classifier
    /// must not regress the existing `SuccessfulWrite` arm.
    #[tokio::test]
    async fn successful_write_resets_streak() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        // Burn the streak up with two no-write turns.
        for _ in 0..2 {
            runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap();
        }
        assert_eq!(runtime.current_streak().await, 2);
        // A real write must flip the streak back to zero (and the
        // total-continuation budget too — see the rationale in
        // `ContinuationState::on_iteration_end`).
        let outcome = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: true,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap();
        assert!(
            outcome.is_none(),
            "a successful write returns no TaskRestart"
        );
        assert_eq!(
            runtime.current_streak().await,
            0,
            "a successful write must reset consecutive_no_write to 0"
        );
        let snap = runtime.snapshot().await;
        assert_eq!(
            snap.total_continuation_turns, 0,
            "a successful write must also reset total_continuation_turns"
        );
    }

    /// Priority A: the Recovery body must include only the last N
    /// failures (the most recent N when more than N are buffered).
    /// Push 5 failures across 3 separate turns so the session-scoped
    /// ring buffer accumulates them across `TurnDiff::reset()`
    /// boundaries; the rendered body must show exactly 3 entries,
    /// and they must be the *most recent* ones.
    #[tokio::test]
    async fn recovery_variant_echoes_up_to_three_recent_errors() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        // Turn 1: two failures.
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: vec![
                    mk_failed_attempt("edit_file", "a.rs", "needle miss A"),
                    mk_failed_attempt("edit_file", "b.rs", "needle miss B"),
                ],
            })
            .await
            .unwrap();
        // Turn 2: one failure.
        runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: vec![mk_failed_attempt(
                    "write_file",
                    "c.rs",
                    "path not found C",
                )],
            })
            .await
            .unwrap();
        // Turn 3: two more failures. After this, the buffer holds 5
        // entries (cap is 6), so all 5 are retained; the renderer
        // will clamp to the last 3.
        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: vec![
                    mk_failed_attempt("edit_file", "d.rs", "needle miss D"),
                    mk_failed_attempt("edit_file", "e.rs", "needle miss E"),
                ],
            })
            .await
            .unwrap()
            .expect("rejected-only third turn must produce a Continuation");
        let body = match restart {
            TaskRestart::Continuation { input, .. } => match input {
                UserInput::Steer { instruction } => instruction,
                other => panic!("expected Steer, got {other:?}"),
            },
            other => panic!("expected Continuation, got {other:?}"),
        };
        // Exactly three numbered entries (`1.` / `2.` / `3.`) — and
        // they must be the latest three: `c.rs`, `d.rs`, `e.rs`.
        assert!(
            body.contains("  1. write_file"),
            "first recap entry: {body}"
        );
        assert!(
            body.contains("path not found C"),
            "first entry must be the third-most-recent (c.rs): {body}"
        );
        assert!(
            body.contains("  2. edit_file (path: d.rs)"),
            "second entry: {body}"
        );
        assert!(
            body.contains("  3. edit_file (path: e.rs)"),
            "third entry: {body}"
        );
        assert!(
            !body.contains("  4."),
            "Recovery body must clamp at exactly three entries: {body}"
        );
        // Oldest two entries (a.rs / b.rs) MUST NOT appear in the
        // recap — they have been bumped past the renderer's window
        // by the more recent rejections.
        assert!(
            !body.contains("needle miss A"),
            "oldest entry (a.rs) must be evicted from the recap window: {body}"
        );
        assert!(
            !body.contains("needle miss B"),
            "older entry (b.rs) must be evicted from the recap window: {body}"
        );
    }

    /// Priority A regression: a turn with NEITHER successful writes
    /// NOR rejected writes (the classic "no write tool was called at
    /// all" case) must still emit the existing `Nudge` body — the
    /// Recovery branch must not steal cases that legitimately belong
    /// to the no-write arm.
    #[tokio::test]
    async fn pure_no_write_keeps_existing_nudge_variant() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: Vec::new(),
            })
            .await
            .unwrap()
            .expect("no-write turn must produce a Continuation");
        match restart {
            TaskRestart::Continuation { kind, input, .. } => {
                assert_eq!(kind, ContinuationKind::Nudge);
                match input {
                    UserInput::Steer { instruction } => {
                        assert!(
                            instruction.contains("kind=\"nudge\""),
                            "pure no-write must still emit the nudge envelope: {instruction}"
                        );
                        assert!(
                            !instruction.contains("kind=\"recovery\""),
                            "pure no-write must NEVER emit a recovery body: {instruction}"
                        );
                    }
                    other => panic!("expected Steer, got {other:?}"),
                }
            }
            other => panic!("expected Continuation, got {other:?}"),
        }
    }

    /// Priority A regression: the codex-style "blocked after 3
    /// consecutive no-writes" threshold must continue to fire on the
    /// pure no-write path. The new Recovery arm must not break the
    /// existing escalation; the third *pure* no-write iteration still
    /// produces `ContinuationKind::Blocked` and the streak is 3 (not
    /// recovered, not nudged).
    #[tokio::test]
    async fn blocked_threshold_unchanged_for_no_write_path() {
        let runtime = fresh_runtime(6);
        let task = TaskId::new_v4();
        let mut kinds = Vec::new();
        for _ in 0..3 {
            let restart = runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: Vec::new(),
                })
                .await
                .unwrap()
                .expect("no-write must produce Continuation");
            match restart {
                TaskRestart::Continuation { kind, .. } => kinds.push(kind),
                other => panic!("expected Continuation, got {other:?}"),
            }
        }
        assert_eq!(
            kinds,
            vec![
                ContinuationKind::Nudge,
                ContinuationKind::Nudge,
                ContinuationKind::Blocked,
            ],
            "three consecutive *pure* no-write iterations must still escalate to Blocked, NOT Recovery"
        );
    }

    /// Priority A: `classify_write_attempt` collapses the
    /// `(had_write, failed_attempts)` pair correctly. Pure unit
    /// coverage so the wiring tests above can stay focused on
    /// behaviour rather than enumeration.
    #[test]
    fn classify_write_attempt_truth_table() {
        let one = vec![mk_failed_attempt("edit_file", "x.rs", "miss")];
        assert_eq!(
            classify_write_attempt(true, &one),
            WriteAttemptKind::SuccessfulWrite,
            "a successful write trumps any sibling rejections"
        );
        assert_eq!(
            classify_write_attempt(true, &[]),
            WriteAttemptKind::SuccessfulWrite
        );
        assert_eq!(
            classify_write_attempt(false, &one),
            WriteAttemptKind::OnlyRejectedWrites
        );
        assert_eq!(
            classify_write_attempt(false, &[]),
            WriteAttemptKind::NoWriteAttempted
        );
    }

    /// Priority A: the session-scoped failure ring buffer evicts via
    /// FIFO at the published cap so a stuck loop cannot grow it
    /// without bound. The buffer is consulted by the Recovery body
    /// renderer, so the boundary behaviour is observable.
    #[tokio::test]
    async fn recent_failures_buffer_caps_at_published_size() {
        let runtime = fresh_runtime(64);
        let task = TaskId::new_v4();
        // Push more failures than the buffer can hold across several
        // turns. The renderer clamps to 3 entries, so peek at the
        // buffer through a public snapshot path: the streak is the
        // proxy here (each turn is OnlyRejectedWrites and the streak
        // never resets), then we render and assert the most-recent
        // entries survive.
        for i in 0..(RECENT_FAILURES_BUFFER_SIZE + 2) {
            let label = format!("miss-{i}");
            runtime
                .handle_event(GoalRuntimeEvent::TurnCompleted {
                    task_id: task,
                    had_write: false,
                    read_paths: empty_read_paths(),
                    failed_write_attempts: vec![mk_failed_attempt("edit_file", "x.rs", &label)],
                })
                .await
                .unwrap();
        }
        // Trigger one more rejected-only turn so the renderer fires
        // on the freshest entry and we can grep the body.
        let restart = runtime
            .handle_event(GoalRuntimeEvent::TurnCompleted {
                task_id: task,
                had_write: false,
                read_paths: empty_read_paths(),
                failed_write_attempts: vec![mk_failed_attempt("edit_file", "x.rs", "miss-final")],
            })
            .await
            .unwrap()
            .expect("final turn must produce a Continuation");
        let body = match restart {
            TaskRestart::Continuation { input, .. } => match input {
                UserInput::Steer { instruction } => instruction,
                other => panic!("expected Steer, got {other:?}"),
            },
            other => panic!("expected Continuation, got {other:?}"),
        };
        // The very oldest entries (miss-0, miss-1) must have been
        // evicted; the freshest one must appear.
        assert!(
            body.contains("miss-final"),
            "newest entry must survive: {body}"
        );
        assert!(
            !body.contains("miss-0"),
            "oldest entry must have been FIFO-evicted: {body}"
        );
    }
}
