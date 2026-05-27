//! Hard-block tool-result body emitted when the dev-loop
//! `implement_now` gate has already fired and the agent is still
//! issuing exploration tools.
//!
//! Distinct from the `SteeringKind::ImplementNow` envelope (which
//! lands as user-channel prose on the **next** turn). This message
//! sits in the rejected exploration tool's `content` field, returned
//! to the model on the **same** turn — see
//! `aura-agent/src/agent_loop/tool_pipeline.rs::partition_circling_duplicate_reads`.

/// `&'static str` body the `implement_now` hard block returns as the
/// rejected tool's `content`.
pub const IMPLEMENT_NOW_HARD_BLOCK_BODY: &str =
    "implement_now has already fired after enough exploration. This read/search tool was blocked; \
     the next action must be write_file, edit_file, delete_file, or task_done with no_changes_needed: true \
     and notes explaining why no file changes are required.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_starts_with_recognisable_token() {
        assert!(IMPLEMENT_NOW_HARD_BLOCK_BODY.starts_with("implement_now"));
        assert!(IMPLEMENT_NOW_HARD_BLOCK_BODY.contains("write_file, edit_file, delete_file"));
        assert!(IMPLEMENT_NOW_HARD_BLOCK_BODY.contains("no_changes_needed"));
    }
}
