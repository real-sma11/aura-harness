//! `<agent_system_prompt>`-bound section.
//!
//! Wraps the operator-authored system prompt in
//! `<agent_system_prompt>...</agent_system_prompt>`. The body is
//! emitted verbatim (trimmed of surrounding whitespace) so the agent
//! sees exactly what the operator wrote.

/// Render the agent-system-prompt section, or `None` when absent / blank.
#[must_use]
pub fn render(prompt: Option<&str>) -> Option<String> {
    let body = prompt?.trim();
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "<agent_system_prompt>\n{body}\n</agent_system_prompt>"
    ))
}
