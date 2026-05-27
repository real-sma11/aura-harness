//! Turn-level configuration: heuristics that determine how the agent loop
//! runs for a given task (complexity classification, token budgets, model
//! selection).
//!
//! NOTE: Previously named `policy`; renamed to `turn_config` to avoid semantic
//! collision with `aura_kernel::policy::Policy` (which is the authorization
//! policy for tool execution). These are turn-level knobs, not authorization.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    Simple,
    Standard,
    Complex,
}

pub fn classify_task_complexity(title: &str, description: &str) -> TaskComplexity {
    let combined = format!("{title} {description}").to_lowercase();
    let mut score: i32 = 0;

    let simple_signals: &[(&str, i32)] = &[
        ("add dependency", -3),
        ("add dep ", -3),
        ("set up dependency", -3),
        ("define enum", -2),
        ("define struct", -2),
        ("define type", -2),
        ("add import", -2),
        ("update cargo.toml", -2),
        ("update package.json", -2),
        ("rename ", -1),
        ("move file", -1),
    ];
    let complex_signals: &[(&str, i32)] = &[
        ("integration test", 3),
        ("end-to-end", 3),
        ("e2e test", 3),
        ("refactor", 3),
        ("migrate", 3),
        ("rewrite", 3),
        ("multi-file", 2),
        ("cross-crate", 2),
        ("implement service", 3),
        ("implement api", 3),
    ];

    for &(pattern, weight) in simple_signals {
        if combined.contains(pattern) {
            score += weight;
        }
    }
    for &(pattern, weight) in complex_signals {
        if combined.contains(pattern) {
            score += weight;
        }
    }

    if description.len() > 1000 {
        score += 2;
    } else if description.len() < 200 {
        score -= 1;
    }

    if score <= -2 {
        TaskComplexity::Simple
    } else if score >= 2 {
        TaskComplexity::Complex
    } else {
        TaskComplexity::Standard
    }
}

/// Member-count-scaled budget escalation.
///
/// Stripped (2026-05): no longer called by `configure_loop_config` — the
/// runner now holds every task at the configured `thinking_budget` floor
/// because the per-complexity escalation translated to "Thought for 2m"
/// bursts rather than faster convergence. Kept around (and unit-tested)
/// so the math is on hand if we want to re-introduce a softer scaling
/// curve later.
#[cfg(test)]
pub fn compute_thinking_budget(base: u32, member_count: usize) -> u32 {
    if member_count >= 15 {
        base.max(16_000)
    } else if member_count >= 8 {
        base.max(10_000)
    } else {
        base
    }
}

/// Returns the model to use for simple tasks. Checks the
/// `aura_config::agent().simple_model_override` (sourced from
/// `AURA_SIMPLE_MODEL` once at startup), falling back to
/// `default_model`.
pub fn resolve_simple_model(default_model: &str) -> String {
    aura_config::agent()
        .simple_model_override
        .clone()
        .unwrap_or_else(|| default_model.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_task_complexity_simple_patterns() {
        assert_eq!(
            classify_task_complexity("Add dependency for serde", ""),
            TaskComplexity::Simple
        );
        assert_eq!(
            classify_task_complexity("Define enum Status", ""),
            TaskComplexity::Simple
        );
        assert_eq!(
            classify_task_complexity("Rename the module", "short"),
            TaskComplexity::Simple
        );
        assert_eq!(
            classify_task_complexity("Update Cargo.toml", ""),
            TaskComplexity::Simple
        );
    }

    #[test]
    fn classify_task_complexity_complex_patterns() {
        assert_eq!(
            classify_task_complexity("Refactor auth module", ""),
            TaskComplexity::Complex
        );
        assert_eq!(
            classify_task_complexity("Add integration test for API", ""),
            TaskComplexity::Complex
        );
        assert_eq!(
            classify_task_complexity("Implement service layer", ""),
            TaskComplexity::Complex
        );
        assert_eq!(
            classify_task_complexity("Migrate to new storage", ""),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn classify_task_complexity_standard_for_moderate_descriptions() {
        let desc = "a".repeat(500);
        assert_eq!(
            classify_task_complexity("Add handler", &desc),
            TaskComplexity::Standard
        );
    }

    #[test]
    fn classify_task_complexity_long_desc_is_complex() {
        let desc = "a".repeat(1500);
        assert_eq!(
            classify_task_complexity("Add handler", &desc),
            TaskComplexity::Complex
        );
    }

    #[test]
    fn compute_thinking_budget_base_for_small_workspace() {
        assert_eq!(compute_thinking_budget(8000, 3), 8000);
    }

    #[test]
    fn compute_thinking_budget_scales_for_medium_workspace() {
        assert_eq!(compute_thinking_budget(8000, 10), 10_000);
    }

    #[test]
    fn compute_thinking_budget_scales_for_large_workspace() {
        assert_eq!(compute_thinking_budget(8000, 20), 16_000);
    }

    /// Shared lock so the two `resolve_simple_model_*` tests don't race
    /// each other on the process-wide `aura_config` singleton.
    fn simple_model_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn resolve_simple_model_uses_default_when_unset() {
        let _guard = simple_model_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cfg = aura_config::current();
        cfg.agent.simple_model_override = None;
        let _scope = aura_config::install_for_test(cfg);
        assert_eq!(resolve_simple_model("test-model"), "test-model");
    }

    #[test]
    fn resolve_simple_model_uses_override_when_set() {
        let _guard = simple_model_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cfg = aura_config::current();
        cfg.agent.simple_model_override = Some("custom-model".to_string());
        let _scope = aura_config::install_for_test(cfg);
        assert_eq!(resolve_simple_model("test-model"), "custom-model");
    }
}
