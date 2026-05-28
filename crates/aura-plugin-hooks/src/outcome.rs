//! Hook handler return values and per-event firing outcomes.
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`HookOutcome`] is closed; adding a variant is a breaking change
//!   for handler authors. The closed-enum invariant test in
//!   `outcome::tests::closed_enum_invariant` matches every variant
//!   without a `_` wildcard so compilation breaks intentionally if a
//!   variant is added without updating callers.
//! - The aggregation rule for multiple handlers on the same event:
//!     1. Handlers run in registration order.
//!     2. The FIRST handler to return [`HookOutcome::Block`] /
//!        [`HookOutcome::Deny`] short-circuits subsequent handlers
//!        for the same event firing.
//!     3. [`HookOutcome::Replace`] mutates the carried value and the
//!        loop continues with the mutated value visible to later
//!        handlers.
//!     4. [`HookOutcome::Approve`] short-circuits the
//!        `PermissionRequest` flow with an auto-approve.
//!     5. [`HookOutcome::TimedOut`] is treated as
//!        [`HookOutcome::Continue`] for control-flow purposes; the
//!        engine logs a `WARN` and counts the timeout in
//!        [`AggregateOutcome::timed_out`].
//!     6. [`HookOutcome::Continue`] is the default observer-only
//!        return; the loop carries on.
//! - Handlers on observer-only events (`PostToolUse`, `Stop`,
//!   `PostCompact`) MAY only return [`HookOutcome::Continue`] /
//!   [`HookOutcome::TimedOut`]. Other variants are silently
//!   downgraded to `Continue` by [`AggregateOutcome::observe`] so a
//!   misconfigured plugin cannot block an observer flow.

use thiserror::Error;

/// Discrete outcome returned by a single hook handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// Handler ran and elected not to mutate the flow.
    Continue,
    /// Handler vetoed the operation. The kernel converts this into
    /// `PolicyVerdict::DeniedByHook` for tool calls; for prompts it
    /// drops the message with a `RecordKind::PromptBlockedByHook`
    /// audit record; for `PreCompact` it skips compaction.
    Block { reason: String },
    /// Handler proposes a replacement value (e.g. a rewritten user
    /// prompt). Only legal on events whose context defines a
    /// mutable payload — currently `UserPromptSubmit`.
    Replace { new_value: String },
    /// Handler short-circuits an interactive `PermissionRequest` with
    /// an auto-approve.
    Approve,
    /// Handler short-circuits an interactive `PermissionRequest` with
    /// an auto-deny.
    Deny { reason: String },
    /// Handler timed out (>5s wall-clock). The engine emits
    /// `tracing::warn!` and treats this as `Continue`.
    TimedOut,
}

impl HookOutcome {
    /// Whether this outcome short-circuits subsequent handlers in
    /// the registered chain for the same event firing.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Block { .. } | Self::Approve | Self::Deny { .. })
    }
}

/// Aggregate outcome of firing all handlers for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateOutcome {
    /// Number of handlers that ran successfully. Includes timed-out
    /// handlers (which are downgraded to `Continue` for control flow
    /// but still counted in [`Self::timed_out`]).
    pub ran: u32,
    /// Number of handlers that hit the wall-clock timeout.
    pub timed_out: u32,
    /// Final aggregate decision. The first terminal outcome wins;
    /// otherwise the last `Replace` wins for replace-eligible events;
    /// otherwise [`HookOutcome::Continue`].
    pub decision: HookOutcome,
}

impl AggregateOutcome {
    /// Default aggregate: zero handlers ran, decision is `Continue`.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            ran: 0,
            timed_out: 0,
            decision: HookOutcome::Continue,
        }
    }

    /// Whether the aggregate decision blocks the firing flow.
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        matches!(self.decision, HookOutcome::Block { .. })
    }

    /// Observer-only downgrade. Drops `Block` / `Replace` / `Approve` /
    /// `Deny` to `Continue` for events where they have no semantic
    /// meaning. Used by `PostToolUse` / `Stop` / `PostCompact`.
    pub fn observe(mut self) -> Self {
        match self.decision {
            HookOutcome::Continue | HookOutcome::TimedOut => {}
            _ => self.decision = HookOutcome::Continue,
        }
        self
    }
}

/// Errors that can be raised by the engine itself (distinct from
/// per-handler [`crate::HookError`]s, which the aggregate loop
/// downgrades to `WARN`).
#[derive(Debug, Error)]
pub enum HookFireError {
    /// Engine refused to fire this event because the runtime is in
    /// a shutdown state.
    #[error("hook engine is shutting down; refusing to fire {event}")]
    ShuttingDown {
        /// Snake_case event name for diagnostics.
        event: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closed-enum invariant: every variant matched explicitly, no
    /// `_` wildcard. Adding a variant breaks this test
    /// (intentional).
    #[test]
    fn closed_enum_invariant() {
        fn label(o: &HookOutcome) -> &'static str {
            match o {
                HookOutcome::Continue => "continue",
                HookOutcome::Block { .. } => "block",
                HookOutcome::Replace { .. } => "replace",
                HookOutcome::Approve => "approve",
                HookOutcome::Deny { .. } => "deny",
                HookOutcome::TimedOut => "timed_out",
            }
        }
        for o in [
            HookOutcome::Continue,
            HookOutcome::Block { reason: "x".into() },
            HookOutcome::Replace {
                new_value: "y".into(),
            },
            HookOutcome::Approve,
            HookOutcome::Deny { reason: "z".into() },
            HookOutcome::TimedOut,
        ] {
            // Each variant must be matched without the wildcard
            // (compile-time guarantee via the `match` above).
            let _ = label(&o);
        }
    }

    #[test]
    fn is_terminal_for_short_circuit_variants() {
        assert!(!HookOutcome::Continue.is_terminal());
        assert!(!HookOutcome::TimedOut.is_terminal());
        assert!(!HookOutcome::Replace {
            new_value: "a".into(),
        }
        .is_terminal());
        assert!(HookOutcome::Block { reason: "x".into() }.is_terminal());
        assert!(HookOutcome::Approve.is_terminal());
        assert!(HookOutcome::Deny { reason: "y".into() }.is_terminal());
    }

    #[test]
    fn observe_downgrades_block_to_continue() {
        let agg = AggregateOutcome {
            ran: 1,
            timed_out: 0,
            decision: HookOutcome::Block { reason: "x".into() },
        };
        let observed = agg.observe();
        assert_eq!(observed.decision, HookOutcome::Continue);
        assert!(!observed.is_blocked());
    }

    #[test]
    fn observe_preserves_continue_and_timed_out() {
        let agg = AggregateOutcome {
            ran: 2,
            timed_out: 1,
            decision: HookOutcome::TimedOut,
        };
        let observed = agg.clone().observe();
        assert_eq!(observed.decision, HookOutcome::TimedOut);
        let agg2 = AggregateOutcome::empty();
        assert_eq!(agg2.observe().decision, HookOutcome::Continue);
    }
}
