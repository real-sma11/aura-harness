//! Model identifier → maximum context-window lookup.
//!
//! Used by [`crate::automaton::AutomatonBridge`] when building the
//! per-run `AgentRunnerConfig` / `AgentIdentity` and by the gateway's
//! chat-session state when caller-selected models land on the wire.
//! Phase B / Commit 3 lifts this out of the gateway-side `session/state.rs`
//! so engine-internal callers don't reach back into `aura-runtime`.

/// Map a model identifier to its maximum context window in tokens.
#[must_use]
pub fn context_window_for_model(model: &str) -> u64 {
    match model {
        m if m.contains("opus-4") => 1_000_000,
        m if m.contains("sonnet-4") => 1_000_000,
        m if m.contains("sonnet-5") => 1_000_000,
        m if m.contains("haiku-4") => 200_000,
        m if m.starts_with("claude") => 200_000,
        m if m.contains("gpt-5.6") || m.contains("gpt-5-6") => 1_050_000,
        m if m.contains("gpt-5.5") || m.contains("gpt-5-5") => 1_050_000,
        m if m.contains("gpt-5.4-mini")
            || m.contains("gpt-5-4-mini")
            || m.contains("gpt-5.4-nano")
            || m.contains("gpt-5-4-nano") =>
        {
            400_000
        }
        m if m.contains("gpt-5.4") || m.contains("gpt-5-4") => 1_050_000,
        m if m.contains("gpt-4.1") => 1_047_576,
        m if m.contains("gpt-4o") || m.contains("gpt-4-turbo") => 128_000,
        m if m.ends_with("-o1") || m.starts_with("o1") => 200_000,
        m if m.contains("-o3") || m.starts_with("o3") => 200_000,
        m if m.contains("-o4") || m.starts_with("o4") => 200_000,
        m if m.contains("deepseek") => 1_000_000,
        m if m.contains("kimi") => 262_144,
        // GLM 5.2 ships a 1M window (earlier GLM tiers were ~200K, which the
        // default below already covers).
        m if m.contains("glm-5.2") || m.contains("glm-5p2") || m.contains("glm-5-2") => 1_000_000,
        _ => 200_000,
    }
}

#[cfg(test)]
mod tests {
    use super::context_window_for_model;

    #[test]
    fn anthropic_aura_aliases() {
        assert_eq!(context_window_for_model("aura-claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("aura-claude-opus-4-6"), 1_000_000);
        assert_eq!(
            context_window_for_model("aura-claude-sonnet-4-6"),
            1_000_000
        );
        assert_eq!(context_window_for_model("aura-claude-sonnet-5"), 1_000_000);
        assert_eq!(context_window_for_model("aura-claude-haiku-4-5"), 200_000);
    }

    #[test]
    fn anthropic_bare_names() {
        assert_eq!(context_window_for_model("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-haiku-4-5"), 200_000);
        assert_eq!(context_window_for_model("claude-3-5-sonnet"), 200_000);
    }

    #[test]
    fn openai_gpt5_aura_aliases() {
        assert_eq!(context_window_for_model("aura-gpt-5-6-sol"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-6-terra"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-6-luna"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-5"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4-mini"), 400_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4-nano"), 400_000);
    }

    #[test]
    fn openai_gpt5_direct_names() {
        assert_eq!(context_window_for_model("gpt-5.6"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.6-sol"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.6-terra"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.6-luna"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.5"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.4"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.4-mini"), 400_000);
        assert_eq!(context_window_for_model("gpt-5.4-nano"), 400_000);
    }

    #[test]
    fn openai_gpt4_and_reasoning() {
        assert_eq!(context_window_for_model("aura-gpt-4.1"), 1_047_576);
        assert_eq!(context_window_for_model("gpt-4.1"), 1_047_576);
        assert_eq!(context_window_for_model("gpt-4o"), 128_000);
        assert_eq!(context_window_for_model("gpt-4-turbo"), 128_000);
        assert_eq!(context_window_for_model("o3"), 200_000);
        assert_eq!(context_window_for_model("aura-o3"), 200_000);
        assert_eq!(context_window_for_model("o4-mini"), 200_000);
        assert_eq!(context_window_for_model("aura-o4-mini"), 200_000);
        assert_eq!(context_window_for_model("o1"), 200_000);
    }

    #[test]
    fn deepseek_and_fireworks() {
        assert_eq!(context_window_for_model("aura-deepseek-v4-pro"), 1_000_000);
        assert_eq!(
            context_window_for_model("aura-deepseek-v4-flash"),
            1_000_000
        );
        assert_eq!(context_window_for_model("deepseek-v4-pro"), 1_000_000);
        assert_eq!(context_window_for_model("aura-kimi-k2-5"), 262_144);
        assert_eq!(context_window_for_model("aura-kimi-k2-6"), 262_144);
        assert_eq!(context_window_for_model("aura-kimi-k2-7-code"), 262_144);
        // GLM 5.2 has a 1M window; earlier GLM tiers fall to the safe default.
        assert_eq!(context_window_for_model("aura-glm-5-2"), 1_000_000);
        assert_eq!(
            context_window_for_model("accounts/fireworks/models/glm-5p2"),
            1_000_000
        );
        assert_eq!(context_window_for_model("aura-glm-5-1"), 200_000);
    }

    #[test]
    fn unknown_model_gets_safe_default() {
        assert_eq!(context_window_for_model("unknown-model-xyz"), 200_000);
    }
}
