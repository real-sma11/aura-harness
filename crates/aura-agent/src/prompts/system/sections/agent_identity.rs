//! `<agent_identity>`-bound section.
//!
//! For PR B this only emits when the caller hands us a non-empty
//! [`AgentIdentity`]. Today no caller does — `agent_runner` passes
//! `None`, the dev-loop / task-run automatons leave `AgenticTaskParams
//! ::agent` at `None`, and the wire fields on `AutomatonStartRequest`
//! deserialise to defaults. The section therefore renders an empty
//! string and `SystemPromptBuilder::agent_identity` is a no-op, which
//! is what the byte-identical PR B snapshots require.
//!
//! PR C will (a) flip the wrapper to the canonical `<agent_identity>
//! ...</agent_identity>` schema, (b) populate the wire fields from
//! `aura-os`, and (c) flow them through into `AgenticTaskParams::agent`
//! so this section starts producing real content.

use crate::prompts::AgentIdentity;

/// Render the agent-identity section.
///
/// Returns `None` (the builder skips empty sections) when the input is
/// `None` or every field is blank. For PR B all production callers fall
/// in that bucket, so this function is exercised only by future PRs and
/// any opt-in unit tests.
#[must_use]
pub(crate) fn render(identity: Option<AgentIdentity<'_>>) -> Option<String> {
    let identity = identity?;
    let name = identity.name.trim();
    let role = identity.role.trim();
    let personality = identity.personality.trim();
    if name.is_empty() && role.is_empty() && personality.is_empty() {
        return None;
    }

    // PR B prose is intentionally placeholder-like: the function is
    // wired but the output is deferred to PR C, which flips every
    // section to the bracketed `<tag>` schema in one intentional
    // snapshot diff. Keeping the body here so the call graph is real.
    let mut out = String::from("\n## Agent Identity\n");
    if !name.is_empty() {
        out.push_str(&format!("- **Name**: {name}\n"));
    }
    if !role.is_empty() {
        out.push_str(&format!("- **Role**: {role}\n"));
    }
    if !personality.is_empty() {
        out.push_str(&format!("- **Personality**: {personality}\n"));
    }
    Some(out)
}
