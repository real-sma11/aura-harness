//! Lifecycle hook event taxonomy.
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`HookEvent`] is a **closed enum** mirroring the 10 lifecycle
//!   events Codex / Claude plugins can subscribe to. Adding a variant
//!   here is a breaking change for plugin authors — every published
//!   plugin's hook list must validate against this enum and skip
//!   (warn-log) any unknown events. The engine performs that
//!   validation at registration time.
//! - The on-disk wire format (manifest [`event`] field, environment
//!   variable [`AURA_EVENT`]) is the **snake_case** spelling. The
//!   `serde(rename_all = "snake_case")` attribute and the
//!   [`HookEvent::as_str`] / [`HookEvent::from_str`] helpers keep
//!   both code paths consistent.
//! - The [`closed_enum_invariant`] test in this module matches every
//!   variant without a `_` wildcard so adding a variant breaks
//!   compilation (intentional).

use serde::{Deserialize, Serialize};

/// Closed enum of the 10 lifecycle events that plugin hooks can
/// subscribe to. Wire format is snake_case (see the module-level docs
/// for the invariant rationale).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Fired before a tool is invoked.
    PreToolUse,
    /// Fired after a tool returns (success or error).
    PostToolUse,
    /// Fired when an interactive session starts.
    SessionStart,
    /// Fired when the user submits a prompt.
    UserPromptSubmit,
    /// Fired when a subagent run starts.
    SubagentStart,
    /// Fired when a subagent run stops.
    SubagentStop,
    /// Fired when the agent loop stops cleanly.
    Stop,
    /// Fired before context compaction begins.
    PreCompact,
    /// Fired after context compaction finishes.
    PostCompact,
    /// Fired when the agent requests an explicit operator permission.
    PermissionRequest,
}

impl HookEvent {
    /// Canonical snake_case wire string for this event.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::Stop => "stop",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::PermissionRequest => "permission_request",
        }
    }

    /// Parse a snake_case wire string into a [`HookEvent`]. Returns
    /// `None` for any input that does not match one of the 10 closed
    /// variants — the engine warn-logs and skips unknown events at
    /// registration time.
    ///
    /// Named `parse_wire` rather than `from_str` to avoid clashing
    /// with the [`std::str::FromStr`] trait method.
    #[must_use]
    pub fn parse_wire(s: &str) -> Option<Self> {
        Some(match s {
            "pre_tool_use" => Self::PreToolUse,
            "post_tool_use" => Self::PostToolUse,
            "session_start" => Self::SessionStart,
            "user_prompt_submit" => Self::UserPromptSubmit,
            "subagent_start" => Self::SubagentStart,
            "subagent_stop" => Self::SubagentStop,
            "stop" => Self::Stop,
            "pre_compact" => Self::PreCompact,
            "post_compact" => Self::PostCompact,
            "permission_request" => Self::PermissionRequest,
            _ => return None,
        })
    }

    /// Full ordered list of every variant. Used by serde round-trip
    /// tests so adding a variant breaks the test until the new entry
    /// is added here too.
    pub const ALL: [Self; 10] = [
        Self::PreToolUse,
        Self::PostToolUse,
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::SubagentStart,
        Self::SubagentStop,
        Self::Stop,
        Self::PreCompact,
        Self::PostCompact,
        Self::PermissionRequest,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closed-enum invariant: every variant matched explicitly, no
    /// `_` wildcard. Adding a variant breaks this test (intentional).
    #[test]
    fn closed_enum_invariant() {
        fn label(e: HookEvent) -> &'static str {
            match e {
                HookEvent::PreToolUse => "pre_tool_use",
                HookEvent::PostToolUse => "post_tool_use",
                HookEvent::SessionStart => "session_start",
                HookEvent::UserPromptSubmit => "user_prompt_submit",
                HookEvent::SubagentStart => "subagent_start",
                HookEvent::SubagentStop => "subagent_stop",
                HookEvent::Stop => "stop",
                HookEvent::PreCompact => "pre_compact",
                HookEvent::PostCompact => "post_compact",
                HookEvent::PermissionRequest => "permission_request",
            }
        }
        for ev in HookEvent::ALL {
            assert_eq!(label(ev), ev.as_str());
        }
    }

    #[test]
    fn all_constant_has_ten_entries() {
        assert_eq!(HookEvent::ALL.len(), 10);
    }

    #[test]
    fn serde_round_trip_every_variant() {
        for ev in HookEvent::ALL {
            let s = serde_json::to_string(&ev).expect("serialize");
            let back: HookEvent = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(back, ev, "round-trip for {ev:?}");
            let expected = format!("\"{}\"", ev.as_str());
            assert_eq!(s, expected, "wire format for {ev:?}");
        }
    }

    #[test]
    fn parse_wire_round_trips() {
        for ev in HookEvent::ALL {
            let back = HookEvent::parse_wire(ev.as_str()).expect("known variant");
            assert_eq!(back, ev);
        }
    }

    #[test]
    fn parse_wire_unknown_returns_none() {
        assert_eq!(HookEvent::parse_wire("definitely_not_an_event"), None);
        assert_eq!(HookEvent::parse_wire(""), None);
    }
}
