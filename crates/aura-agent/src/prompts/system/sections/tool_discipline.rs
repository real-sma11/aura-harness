//! `<tool_discipline>`-bound section.
//!
//! The historical tool-call discipline prose was deleted by the 2026-05
//! cook-loop strip alongside the runtime safety valves it described
//! (chunk guard, narration budget, force-tool-next-turn hint). PR C
//! keeps the section module wired into [`super::super::SystemPromptBuilder`]
//! so the canonical section ordering mirrors the schema in the plan,
//! but [`render`] still returns `None` so the section emits no bytes
//! today. When future prose returns, wrap it in
//! `<tool_discipline>...</tool_discipline>`.
//!
//! The builder drops `None` sections, so the assembled prompt stays
//! free of an empty `<tool_discipline></tool_discipline>` tag.

/// Render the tool-discipline section, or `None` (the current state).
#[must_use]
pub(crate) fn render() -> Option<String> {
    None
}
