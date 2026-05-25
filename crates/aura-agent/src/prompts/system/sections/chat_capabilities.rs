//! `<chat_capabilities>`-bound section.
//!
//! Hosts the prose that historically lived in
//! `prompts::system::CHAT_SYSTEM_PROMPT_BASE` — the "You are Aura, an
//! AI software engineering assistant ..." block consumed by chat-path
//! builders. For PR B [`render`] returns the legacy constant verbatim;
//! the constant itself stays declared in [`super::super`] so external
//! re-exports (`crate::prompts::CHAT_SYSTEM_PROMPT_BASE`) continue to
//! resolve unchanged. PR C will (a) flip the wrapper to
//! `<chat_capabilities>...</chat_capabilities>` and (b) move the
//! constant ownership here.

/// Render the chat-capabilities section verbatim. Always non-empty.
#[must_use]
pub(crate) fn render() -> String {
    super::super::CHAT_SYSTEM_PROMPT_BASE.to_string()
}
