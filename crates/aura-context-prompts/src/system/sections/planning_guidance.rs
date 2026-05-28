//! `<planning_guidance>`-bound section.
//!
//! Codex-derived plan-tool guidance plus aura's `submit_plan`
//! contract. Dev-loop path only — chat carries its own planning flow
//! in [`super::chat_capabilities::CHAT_SYSTEM_PROMPT_BASE`].

/// Render the planning-guidance section wrapped in the canonical envelope.
#[must_use]
pub fn render() -> Option<String> {
    let body = "\
When using planning / `submit_plan`:
- Skip planning for straightforward tasks (roughly the easiest 25%).
- Do not make single-step plans.
- When you made a plan, update it after performing one of the sub-tasks you shared on the plan.
- `submit_plan` is transparent commitment surfaced to the operator, not a gate — writes are accepted with or without it.";
    Some(format!("<planning_guidance>\n{body}\n</planning_guidance>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_envelope_with_body() {
        let out = render().expect("planning_guidance section renders");
        assert!(out.starts_with("<planning_guidance>\n"));
        assert!(out.ends_with("\n</planning_guidance>"));
        assert!(!out.contains("<planning_guidance></planning_guidance>"));
    }

    #[test]
    fn render_includes_submit_plan_and_skip_easy_tasks() {
        let out = render().expect("section renders");
        assert!(out.contains("submit_plan"));
        assert!(out.contains("easiest 25%"));
    }
}
