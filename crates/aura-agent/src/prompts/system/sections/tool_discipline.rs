//! `<tool_discipline>`-bound section.
//!
//! The historical tool-call discipline prose was deleted by the 2026-05
//! cook-loop strip alongside the runtime safety valves it described
//! (chunk guard, narration budget, force-tool-next-turn hint). PR B
//! keeps the section module wired into [`super::super::SystemPromptBuilder`]
//! so the call graph mirrors the canonical schema, but [`render`]
//! returns `None` so the section emits no bytes today.
//!
//! Future PRs can resurrect content here without touching every call
//! site if a discipline section ever returns.

/// Render the tool-discipline section, or `None` (the current state).
#[must_use]
pub(crate) fn render() -> Option<String> {
    None
}