//! `<agent_skills>`-bound section.
//!
//! Wraps the skills list in `<agent_skills>...</agent_skills>`.
//! Returns `None` (the builder drops the section) when the list is
//! empty or contains only whitespace entries.

/// Render the agent-skills section, or `None` when the list is empty.
#[must_use]
pub fn render(skills: &[String]) -> Option<String> {
    let mut filtered = skills.iter().map(|s| s.trim()).filter(|s| !s.is_empty());
    let first = filtered.next()?;

    let mut body = String::new();
    body.push_str(&format!("- {first}\n"));
    for skill in filtered {
        body.push_str(&format!("- {skill}\n"));
    }
    Some(format!("<agent_skills>\n{body}</agent_skills>"))
}
