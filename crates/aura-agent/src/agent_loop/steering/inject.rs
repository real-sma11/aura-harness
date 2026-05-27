//! Agent-owned helper that renders a [`SteeringKind`] via
//! [`SteeringRenderer`] and appends the wrapped envelope to a live
//! `Vec<aura_reasoner::Message>` user-message stream.
//!
//! `aura-prompts` cannot perform this append itself: its boundary
//! contract forbids both the reasoner dep and `Vec<Message>`
//! mutation. So the rendering is delegated to
//! `aura_prompts::SteeringRenderer::render` (a pure `String`
//! producer) and the appending lives here, where it has direct
//! access to the message-mutation helper [`crate::helpers::append_warning`].

use aura_prompts::{SteeringKind, SteeringRenderer};
use aura_reasoner::Message;

use crate::helpers;

/// Render `kind` and append the resulting envelope to the live
/// user-message stream via [`helpers::append_warning`]. Returns the
/// wrapped string so callers can also emit it on a stream channel.
pub fn inject(messages: &mut Vec<Message>, kind: SteeringKind) -> String {
    let wrapped = SteeringRenderer::render(&kind);
    helpers::append_warning(messages, &wrapped);
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_reasoner::{ContentBlock, Role};

    /// Build the canonical envelope opener for `task_done_rejected`
    /// without hardcoding the literal. The `<harness_steering` token
    /// must only ever appear inside `aura-prompts`; the workspace
    /// guardrail test in `aura-prompts/src/steering/tests.rs` greps
    /// every other crate for that literal and fails CI if it appears.
    /// Tests in this module derive the expected text from
    /// [`SteeringRenderer::render`] so the assertions stay in
    /// lockstep with the renderer without copying the marker bytes.
    fn task_done_envelope_opener() -> String {
        let full = SteeringRenderer::render(&SteeringKind::TaskDoneNoWrites);
        full.lines()
            .next()
            .expect("envelope opener line")
            .to_string()
    }

    #[test]
    fn appends_envelope_to_user_message_via_append_warning() {
        let mut messages = vec![Message::user("hello")];
        let returned = inject(&mut messages, SteeringKind::TaskDoneNoWrites);

        let opener = task_done_envelope_opener();
        assert!(
            returned.starts_with(&opener),
            "expected envelope-wrapped body, got:\n{returned}"
        );
        assert_eq!(
            messages.len(),
            1,
            "append_warning should fold into existing user message, not push a new one"
        );
        assert_eq!(messages[0].role, Role::User);
        let combined = messages[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            combined.contains("hello"),
            "original user content should be preserved:\n{combined}"
        );
        assert!(
            combined.contains(&opener),
            "envelope should be appended to the user message:\n{combined}"
        );
    }

    #[test]
    fn after_assistant_message_pushes_new_user_message() {
        let mut messages = vec![Message::assistant("hi")];
        let _returned = inject(&mut messages, SteeringKind::TaskDoneNoWrites);

        assert_eq!(
            messages.len(),
            2,
            "an assistant tail must be followed by a fresh user message carrying the envelope"
        );
        assert_eq!(messages[1].role, Role::User);
        let combined = messages[1]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let opener = task_done_envelope_opener();
        assert!(
            combined.contains(&opener),
            "envelope should land in a fresh user message:\n{combined}"
        );
    }
}
