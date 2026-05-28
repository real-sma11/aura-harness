//! Tool-predicate helpers + reasoner message helpers shared by the
//! steering evaluators.
//!
//! Relocated from `aura-agent::helpers` in Phase 6a. `aura-agent`
//! re-exports `append_warning`, `is_exploration_tool`, and
//! `is_write_tool` so existing call sites are unchanged.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use aura_reasoner::{ContentBlock, Message, Role};

/// Append a warning as a text block to the last user message, or
/// push a new user message if the last message isn't a user
/// message.
///
/// This is safe to call after `tool_result` messages because it
/// appends to the existing user message rather than inserting a new
/// one that would break the `tool_use/tool_result` adjacency
/// required by Anthropic.
pub fn append_warning(messages: &mut Vec<Message>, warning: &str) {
    if let Some(last) = messages.last_mut() {
        if last.role == Role::User {
            last.content.push(ContentBlock::Text {
                text: warning.to_string(),
            });
            return;
        }
    }
    messages.push(Message::user(warning));
}

/// Check if a tool name is a write tool (mutation).
#[must_use]
pub fn is_write_tool(name: &str) -> bool {
    aura_config::WRITE_TOOLS.contains(&name)
}

/// Check if a tool name is an exploration tool (read-only).
#[must_use]
pub fn is_exploration_tool(name: &str) -> bool {
    aura_config::EXPLORATION_TOOLS.contains(&name)
}

/// Stable hex digest of a byte payload, used by the steering layer's
/// `(content_hash → count)` repeat tracker and (indirectly) by the
/// agent loop's `tool_execution` to attach `content_hash` to read
/// results before the per-turn tracker observes them.
///
/// The hash function is intentionally `std::collections::hash_map::DefaultHasher`
/// so the digest is per-run stable but NOT cryptographically robust
/// — Phase 6a treats it as an identity probe for "is this exactly
/// the same bytes the agent just read?" rather than as a security
/// signal.
#[must_use]
pub fn content_hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
