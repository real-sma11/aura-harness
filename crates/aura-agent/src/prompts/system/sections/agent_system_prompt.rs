//! `<agent_system_prompt>`-bound section.
//!
//! Renders the operator-authored system prompt verbatim when present,
//! otherwise emits nothing. Like the other identity-related sections,
//! every PR B production call site passes `None` so the builder output
//! stays byte-identical with PR A.

/// Render the agent-system-prompt section, or `None` when absent / blank.
#[must_use]
pub(crate) fn render(prompt: Option<&str>) -> Option<String> {
    let body = prompt?.trim();
    if body.is_empty() {
        return None;
    }
    let mut out = String::from("\n## Agent System Prompt\n");
    out.push_str(body);
    out.push('\n');
    Some(out)
}
