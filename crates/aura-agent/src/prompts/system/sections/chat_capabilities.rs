//! `<chat_capabilities>`-bound section.
//!
//! PR C wraps the historical `CHAT_SYSTEM_PROMPT_BASE` prose in the
//! canonical `<chat_capabilities>...</chat_capabilities>` envelope so
//! the chat-path system prompt mirrors the dev-loop path's bracketed
//! schema. The constant itself still lives in [`super::super`] so
//! external re-exports (`crate::prompts::CHAT_SYSTEM_PROMPT_BASE`)
//! resolve unchanged.

/// Render the chat-capabilities section verbatim. Always non-empty.
#[must_use]
pub(crate) fn render() -> String {
    format!(
        "<chat_capabilities>\n{body}\n</chat_capabilities>",
        body = super::super::CHAT_SYSTEM_PROMPT_BASE,
    )
}
