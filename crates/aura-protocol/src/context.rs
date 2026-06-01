//! Rendered text for each static context bucket, emitted alongside the
//! per-bucket token counts in [`crate::server::ContextBreakdown`].
//!
//! Where `ContextBreakdown` answers "how many tokens does each bucket
//! cost", [`ContextContents`] answers "what is the actual text the
//! model receives in each bucket". The two travel together on
//! [`crate::server::SessionUsage`] so a client can render a
//! context-bucket viewer with the real prompt/tool/skill/subagent text
//! rather than just stacked-bar token estimates.
//!
//! Strictly additive on the wire: older harness builds omit
//! [`crate::server::SessionUsage::context_contents`] entirely
//! (`Option::None`), and older clients ignore the field on
//! deserialize.

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

/// Rendered text plus token estimate for a single context entry — one
/// tool, one skill, or one subagent kind.
///
/// `tokens` uses the same `chars / CHARS_PER_TOKEN` heuristic as the
/// matching bucket in [`crate::server::ContextBreakdown`], so the
/// per-segment numbers stay directly comparable to the bucket total.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ContextSegment {
    /// Short human-readable label (tool name, skill name, subagent kind).
    pub label: String,
    /// Full rendered text the model receives for this entry.
    pub text: String,
    /// Estimated token cost of [`Self::text`].
    pub tokens: u64,
}

/// Actual rendered text for each static context bucket, parallel to the
/// token counts in [`crate::server::ContextBreakdown`].
///
/// - `system_prompt` — the full rendered system prompt (already
///   includes any injected skill text). `None` when the prompt is
///   empty.
/// - `tools` — one segment per tool the request would carry.
/// - `skills` — one segment per installed skill (name + summary).
/// - `subagents` — one segment per registered subagent kind.
/// - `mcp` — reserved for MCP server context; empty today, mirroring
///   the zeroed `mcp_tokens` bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ContextContents {
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<ContextSegment>,
    #[serde(default)]
    pub skills: Vec<ContextSegment>,
    #[serde(default)]
    pub subagents: Vec<ContextSegment>,
    #[serde(default)]
    pub mcp: Vec<ContextSegment>,
}

impl ContextContents {
    /// True when no bucket carries any content. The wire boundary uses
    /// this to leave [`crate::server::SessionUsage::context_contents`]
    /// `None` for empty turns so pre-upgrade-style omitted payloads and
    /// genuinely empty turns look identical on the wire.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.system_prompt.is_none()
            && self.tools.is_empty()
            && self.skills.is_empty()
            && self.subagents.is_empty()
            && self.mcp.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ContextContents, ContextSegment};

    #[test]
    fn is_empty_true_for_default() {
        assert!(ContextContents::default().is_empty());
    }

    #[test]
    fn is_empty_false_when_any_bucket_populated() {
        let contents = ContextContents {
            system_prompt: Some("you are a helpful agent".to_string()),
            ..ContextContents::default()
        };
        assert!(!contents.is_empty());

        let contents = ContextContents {
            tools: vec![ContextSegment {
                label: "read_file".to_string(),
                text: "Read a file from disk".to_string(),
                tokens: 5,
            }],
            ..ContextContents::default()
        };
        assert!(!contents.is_empty());
    }

    /// Serialization round-trip for the persisted/wire type: a fully
    /// populated value must survive `to_string` → `from_str` byte-for-
    /// byte at the value level.
    #[test]
    fn context_contents_round_trips_through_json() {
        let original = ContextContents {
            system_prompt: Some("system prompt text".to_string()),
            tools: vec![ContextSegment {
                label: "read_file".to_string(),
                text: "read_file\n\nRead a file.\n\n{}".to_string(),
                tokens: 7,
            }],
            skills: vec![ContextSegment {
                label: "deploy".to_string(),
                text: "Deploy the app".to_string(),
                tokens: 3,
            }],
            subagents: vec![ContextSegment {
                label: "explore".to_string(),
                text: "Read-only exploration subagent".to_string(),
                tokens: 6,
            }],
            mcp: Vec::new(),
        };

        let json = serde_json::to_string(&original).expect("serialize ContextContents");
        let decoded: ContextContents =
            serde_json::from_str(&json).expect("deserialize ContextContents");
        assert_eq!(original, decoded);
    }

    /// Tolerant parsing: an empty object must decode to the all-empty
    /// default thanks to the per-field `#[serde(default)]`, so older
    /// senders that omit buckets deserialize cleanly.
    #[test]
    fn empty_object_decodes_to_default() {
        let decoded: ContextContents =
            serde_json::from_str("{}").expect("deserialize empty object");
        assert_eq!(decoded, ContextContents::default());
        assert!(decoded.is_empty());
    }
}
