//! Budget tracking — exploration, token, and credit budget management.

use crate::constants::{BUDGET_WARNING_30, BUDGET_WARNING_40_NO_WRITE, BUDGET_WARNING_60};

/// Budget tracking state.
#[derive(Debug, Default)]
pub struct BudgetState {
    /// Whether the 30% warning has been sent.
    pub warned_30: bool,
    /// Whether the 40% no-write warning has been sent.
    pub warned_40_no_write: bool,
    /// Whether the 60% warning has been sent.
    pub warned_60: bool,
}

/// Exploration tracking state.
///
/// The "approaching budget" warning fields were removed by the
/// cook-loop-fix strip (2026-05) along with the
/// `EXPLORATION_WARNING_*_OFFSET` constants. The remaining `count`
/// field tracks total exploration tool calls for telemetry only —
/// the hard exploration block and its allowance threading were
/// stripped along with `compute_exploration_allowance`.
#[derive(Debug, Default)]
pub struct ExplorationState {
    /// Total exploration tool calls.
    pub count: usize,
}

/// Check if a budget warning should be injected, returning the message if so.
pub fn check_budget_warning(
    budget: &mut BudgetState,
    utilization: f64,
    had_any_write: bool,
) -> Option<String> {
    if utilization >= BUDGET_WARNING_60 && !budget.warned_60 {
        budget.warned_60 = true;
        return Some(
            "WARNING: You have used over 60% of your iteration budget. \
             Wrap up immediately. Complete your current changes and stop."
                .to_string(),
        );
    }

    if utilization >= BUDGET_WARNING_40_NO_WRITE && !had_any_write && !budget.warned_40_no_write {
        budget.warned_40_no_write = true;
        return Some(
            "CRITICAL WARNING: You have used 40% of your budget without making ANY writes. \
             Stop exploring and start implementing immediately with what you know."
                .to_string(),
        );
    }

    if utilization >= BUDGET_WARNING_30 && !budget.warned_30 {
        budget.warned_30 = true;
        return Some(
            "NOTE: You have used 30% of your iteration budget. \
             Prioritize implementing your solution over further exploration."
                .to_string(),
        );
    }

    None
}

/// Check if the budget has been exceeded and the loop should stop.
///
/// `max_iterations == usize::MAX` is treated as "unlimited" — the
/// iteration check short-circuits and termination is driven solely by
/// the credit budget (when set). Callers using a wire-protocol u32
/// (e.g. `aura_runtime::session::state::Session::max_turns`) should
/// map `u32::MAX` → `usize::MAX` before reaching this function.
pub const fn should_stop_for_budget(
    iteration: usize,
    max_iterations: usize,
    avg_tokens_per_iteration: u64,
    total_tokens: u64,
    credit_budget: Option<u64>,
) -> bool {
    if let Some(budget) = credit_budget {
        if total_tokens + avg_tokens_per_iteration > budget {
            return true;
        }
    }

    if max_iterations == usize::MAX {
        return false;
    }

    iteration >= max_iterations.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_warning_30pct() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.31, true);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("30%"));
        assert!(budget.warned_30);
    }

    #[test]
    fn test_budget_warning_60pct() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.61, true);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("60%"));
    }

    #[test]
    fn test_no_write_warning_at_40pct() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.41, false);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("40%"));
    }

    #[test]
    fn test_no_write_warning_skipped_after_write() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.41, true);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("30%"));
        assert!(!budget.warned_40_no_write);
    }

    #[test]
    fn test_should_stop_for_budget() {
        assert!(should_stop_for_budget(24, 25, 1000, 0, None));
        assert!(!should_stop_for_budget(10, 25, 1000, 0, None));
        assert!(should_stop_for_budget(5, 25, 1000, 9500, Some(10000)));
    }

    #[test]
    fn test_should_stop_at_exact_max_iterations() {
        assert!(should_stop_for_budget(24, 25, 0, 0, None));
        assert!(!should_stop_for_budget(23, 25, 0, 0, None));
    }

    #[test]
    fn test_should_stop_credit_budget_exact_boundary() {
        assert!(should_stop_for_budget(1, 25, 500, 9600, Some(10000)));
        assert!(!should_stop_for_budget(1, 25, 500, 9400, Some(10000)));
    }

    #[test]
    fn test_should_stop_no_budget_only_iterations() {
        assert!(!should_stop_for_budget(0, 25, 0, 0, None));
        assert!(should_stop_for_budget(24, 25, 0, 999_999, None));
    }

    #[test]
    fn test_unlimited_max_iterations_never_stops_for_iterations() {
        // `usize::MAX` is the sentinel for "unlimited iterations" set
        // by `aura_agent::constants::MAX_ITERATIONS` (default) and by
        // `aura_runtime` when the wire-protocol `max_turns == u32::MAX`.
        // The function must never report a stop based on the iteration
        // counter alone — even at very high iteration values.
        assert!(!should_stop_for_budget(0, usize::MAX, 0, 0, None));
        assert!(!should_stop_for_budget(1_000, usize::MAX, 0, 0, None));
        assert!(!should_stop_for_budget(
            usize::MAX - 2,
            usize::MAX,
            0,
            0,
            None
        ));
        // The credit budget still terminates the loop in unlimited mode.
        assert!(should_stop_for_budget(
            42,
            usize::MAX,
            500,
            9_600,
            Some(10_000)
        ));
        assert!(!should_stop_for_budget(
            42,
            usize::MAX,
            500,
            9_400,
            Some(10_000)
        ));
    }

    #[test]
    fn test_budget_warnings_idempotent() {
        let mut budget = BudgetState::default();
        let msg1 = check_budget_warning(&mut budget, 0.31, true);
        assert!(msg1.is_some());
        let msg2 = check_budget_warning(&mut budget, 0.35, true);
        assert!(msg2.is_none(), "Should not repeat 30% warning");
    }

    #[test]
    fn test_budget_warning_60_overrides_30() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.65, true);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("60%"));
    }

    #[test]
    fn test_budget_no_warning_below_30() {
        let mut budget = BudgetState::default();
        let msg = check_budget_warning(&mut budget, 0.25, true);
        assert!(msg.is_none());
    }

    #[test]
    fn test_exploration_state_defaults() {
        let state = ExplorationState::default();
        assert_eq!(state.count, 0);
    }
}
