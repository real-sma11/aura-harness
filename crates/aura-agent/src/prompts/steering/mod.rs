//! Per-iteration model-facing steering messages.
//!
//! Every per-iteration injection the harness emits (tool-result
//! rejections from the `task_executor` gates, stub-detector
//! rejections, …) flows through
//! [`SteeringInjector`] and ends up wrapped in
//! `<harness_steering kind="...">...</harness_steering>` in the
//! transcript. The kind label lets the model pattern-match the
//! envelope when deciding how to react, and routing every body
//! through [`messages::render`] keeps every model-facing string the
//! harness emits under `crates/aura-agent/src/prompts/`.
//!
//! ## Surface
//!
//! - [`SteeringKind`]: tagged union of the active steering kinds.
//!   Variants exist only for call sites the audit (PR D Step 1)
//!   confirmed are still live; the original PR D plan also listed
//!   `EndturnWithoutWrite`, `ReadOnlyStreak`, and `NarrationBudget`,
//!   but those injection paths were already deleted by concurrent
//!   commits (`955d05b`, `91be64f`, `91e6f7d`) before PR D landed, so
//!   they are intentionally absent here. Re-adding them would
//!   re-introduce behaviour those commits deliberately removed.
//! - [`SteeringInjector::inject`]: wraps the rendered body in the
//!   envelope, appends it to the live user-message stream via
//!   [`crate::helpers::append_warning`], and returns the wrapped
//!   string. Used by per-iteration injection sites (none survive
//!   today — kept on the surface for future steering kinds that need
//!   to land in `state.messages`).
//! - [`SteeringInjector::render`]: wraps the rendered body in the
//!   envelope and returns the string without touching any message
//!   list. Used by tool-result rejection sites (the surviving live
//!   sites in `task_executor::handlers`) whose body is carried back
//!   to the model via the tool-result channel instead of an
//!   append-to-user injection.
//!
//! The renderer functions live in [`messages`] and accept the
//! data each kind needs. The wording is preserved verbatim from the
//! pre-PR-D inline `format!` blocks; PR D only adds the envelope.

mod early_oracle;
mod implement_now_gate;
mod injector;
mod messages;
mod repeated_read;

#[cfg(test)]
mod tests;

pub use early_oracle::EarlyTestOracle;
pub use implement_now_gate::{evaluate_implement_now, IMPLEMENT_NOW_DEFAULT_THRESHOLD};
pub use injector::{SteeringInjector, SteeringKind};
pub use repeated_read::{RepeatedReadTracker, REPEATED_READ_THRESHOLD};
