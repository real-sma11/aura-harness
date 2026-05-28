//! `<frontend_design>`-bound section.
//!
//! Codex-derived frontend design discipline (anti-AI-slop guidance).
//! Applies to both dev-loop and chat paths; conditionally relevant
//! for non-UI work.

/// Render the frontend-design section wrapped in the canonical envelope.
#[must_use]
pub fn render() -> Option<String> {
    let body = "\
When doing frontend design tasks, avoid collapsing into generic \"AI slop\" or safe, average-looking layouts. Aim for interfaces that feel intentional, bold, and a bit surprising.
- Typography: use expressive, purposeful fonts; avoid default stacks (Inter, Roboto, Arial, system).
- Color and look: choose a clear visual direction; define CSS variables; avoid purple-on-white defaults; no purple bias or dark-mode bias.
- Motion: use a few meaningful animations (page-load, staggered reveals) instead of generic micro-motions.
- Background: do not rely on flat single-color backgrounds; use gradients, shapes, or subtle patterns to build atmosphere.
- Variety: avoid boilerplate layouts and interchangeable UI patterns; vary themes, type families, and visual languages across outputs.
- Responsive: ensure the page loads properly on both desktop and mobile.
- Exception: when working within an existing website or design system, preserve the established patterns, structure, and visual language.";
    Some(format!("<frontend_design>\n{body}\n</frontend_design>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_envelope_with_body() {
        let out = render().expect("frontend_design section renders");
        assert!(out.starts_with("<frontend_design>\n"));
        assert!(out.ends_with("\n</frontend_design>"));
        assert!(!out.contains("<frontend_design></frontend_design>"));
    }

    #[test]
    fn render_includes_typography_and_exception_rules() {
        let out = render().expect("section renders");
        assert!(out.contains("Typography"));
        assert!(out.contains("Exception"));
        assert!(out.contains("design system"));
    }
}
