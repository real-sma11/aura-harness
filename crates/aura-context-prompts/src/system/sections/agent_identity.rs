//! `<agent_identity>`-bound section.
//!
//! Renders a minimal key/value listing (`name:` / `role:` /
//! `personality:`); blank fields are omitted so legacy / partial
//! identity rows produce a short, readable payload. Returns `None`
//! (the builder skips empty sections) when the input is `None` or
//! every field is blank.

use crate::descriptors::AgentIdentity;

/// Render the agent-identity section.
///
/// Returns `None` (the builder drops empty sections) when the input is
/// `None` or every field is blank.
#[must_use]
pub fn render(identity: Option<AgentIdentity<'_>>) -> Option<String> {
    let identity = identity?;
    let name = identity.name.trim();
    let role = identity.role.trim();
    let personality = identity.personality.trim();
    if name.is_empty() && role.is_empty() && personality.is_empty() {
        return None;
    }

    let mut body = String::new();
    if !name.is_empty() {
        body.push_str(&format!("name: {name}\n"));
    }
    if !role.is_empty() {
        body.push_str(&format!("role: {role}\n"));
    }
    if !personality.is_empty() {
        body.push_str(&format!("personality: {personality}\n"));
    }
    Some(format!("<agent_identity>\n{body}</agent_identity>"))
}
