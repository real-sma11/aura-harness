//! `<agent_skills>`-bound section.
//!
//! Empty-list = no output, which is what every PR B caller currently
//! produces (the wire field defaults to `Vec::new()` and never gets
//! populated until PR C). The section module exists so the wire and
//! builder API land in this PR while the rendered bytes still match
//! the PR A snapshots.

/// Render the agent-skills section, or `None` when the list is empty.
#[must_use]
pub(crate) fn render(skills: &[String]) -> Option<String> {
    let mut filtered = skills
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let first = filtered.next()?;

    let mut out = String::from("\n## Agent Skills\n");
    out.push_str(&format!("- {first}\n"));
    for skill in filtered {
        out.push_str(&format!("- {skill}\n"));
    }
    Some(out)
}
