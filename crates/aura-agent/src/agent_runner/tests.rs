use super::*;

#[test]
fn configure_loop_config_simple_caps_max_tokens() {
    let config = AgentRunnerConfig::for_agent("claude-test-model");
    let loop_cfg = configure_loop_config(TaskComplexity::Simple, &config, 3, "system".into());
    assert!(loop_cfg.max_tokens <= 8_192);
    assert!(loop_cfg.max_iterations <= 15);
}

#[test]
fn configure_loop_config_complex_uses_full_budget() {
    let config = AgentRunnerConfig::for_agent("claude-test-model");
    let loop_cfg = configure_loop_config(TaskComplexity::Complex, &config, 3, "system".into());
    assert_eq!(
        loop_cfg.max_tokens,
        config.task_execution_max_tokens.max(32_768)
    );
    assert_eq!(loop_cfg.max_iterations, config.max_agentic_iterations);
}

#[test]
fn configure_loop_config_maps_all_fields() {
    let config = AgentRunnerConfig::for_agent("claude-test-model");
    let loop_cfg = configure_loop_config(TaskComplexity::Standard, &config, 3, "system".into());
    assert_eq!(loop_cfg.billing_reason, "aura_task");
    assert_eq!(loop_cfg.auto_build_cooldown, 1);
}

#[test]
fn configure_loop_config_seeds_thinking_budget() {
    // The runner derives a complexity-adjusted `thinking_budget` and
    // wires it through `AgentLoopConfig::thinking_budget` so the
    // agent-loop's `LoopState::thinking.budget` starts there (rather
    // than at `max_tokens`). Without this, the per-task tuning in
    // `compute_thinking_budget` would silently no-op.
    let config = AgentRunnerConfig {
        thinking_budget: 4_000,
        task_execution_max_tokens: 16_384,
        ..AgentRunnerConfig::for_agent("claude-test-model")
    };
    let standard = configure_loop_config(TaskComplexity::Standard, &config, 3, "system".into());
    assert_eq!(standard.thinking_budget, Some(4_000));

    let simple = configure_loop_config(TaskComplexity::Simple, &config, 3, "system".into());
    let simple_budget = simple.thinking_budget.expect("seeded");
    assert!(
        simple_budget <= simple.max_tokens,
        "must respect max_tokens ceiling"
    );
    assert!(simple_budget <= 4_000);
}

#[test]
fn check_repeated_error_returns_none_on_first() {
    let result = check_repeated_error(&[], "sig1", 1, "cargo build");
    assert!(result.is_none());
}

#[test]
fn check_repeated_error_triggers_after_three_dupes() {
    let prior = vec![
        BuildFixAttemptRecord {
            stderr: "err".into(),
            error_signature: "sig1".into(),
            files_changed: vec![],
            changes_summary: String::new(),
        },
        BuildFixAttemptRecord {
            stderr: "err".into(),
            error_signature: "sig1".into(),
            files_changed: vec![],
            changes_summary: String::new(),
        },
    ];
    let result = check_repeated_error(&prior, "sig1", 3, "cargo build");
    assert!(result.is_some());
}

#[test]
fn finalize_loop_result_uses_text_when_present() {
    let result = AgentLoopResult {
        total_text: "Did the thing".to_string(),
        total_input_tokens: 100,
        total_output_tokens: 50,
        ..AgentLoopResult::default()
    };
    let exec = finalize_loop_result(result);
    assert_eq!(exec.notes, "Did the thing");
    assert_eq!(exec.input_tokens, 100);
    assert_eq!(exec.output_tokens, 50);
}

#[test]
fn finalize_loop_result_default_notes_when_empty() {
    let result = AgentLoopResult::default();
    let exec = finalize_loop_result(result);
    assert!(exec.notes.contains("agentic tool-use loop"));
}
