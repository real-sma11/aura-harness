//! Phase 1.B continuation runtime — re-export shim (Layer E.4).
//!
//! The canonical continuation logic moved to
//! [`crate::session::goal_runtime`] in Layer E.4 so the streak counter
//! lives on the *session* and survives task restarts (codex parity).
//! This module is preserved as a `pub(crate)` re-export shim so
//! existing call sites (`crate::agent_loop::LoopState::continuation`,
//! tests, dev-loop log greps) keep compiling during the migration.
//!
//! New code MUST import from [`crate::session::goal_runtime`]
//! directly. Anything that still reaches into `agent_loop::continuation`
//! is one of:
//!
//! - the shadow `LoopState::continuation` field that the dev-loop
//!   reasoning-effort policy (`compute_thinking_effort`) reads (the
//!   field is fed from `GoalRuntime` snapshots in the turn stop
//!   hook),
//! - the migrated unit tests, which now exercise the canonical
//!   `GoalRuntime` instead.

pub(crate) use crate::session::goal_runtime::ContinuationState;
