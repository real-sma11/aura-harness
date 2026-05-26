//! Message sanitization — repairs message history for API validity.
//!
//! Runs 6 passes:
//! 1. Remove empty messages
//! 2. Merge consecutive same-role messages
//! 3. Fix orphan tool results (`tool_result` without matching `tool_use`)
//! 4. Fix unpaired tool uses (`tool_use` without matching `tool_result`)
//! 5. Ensure conversation starts with a user message
//! 6. Assert positional `tool_use/tool_result` constraint (debug guard)

use crate::dup_audit;
use aura_reasoner::{ContentBlock, Message, Role, ToolResultContent};
use std::collections::HashSet;
use tracing::warn;

/// Run all sanitization passes on the message history.
pub fn validate_and_repair(messages: &mut Vec<Message>) {
    dup_audit::audit_tool_result_duplicates(messages, "sanitize.entry");
    remove_empty_messages(messages);
    merge_consecutive_same_role(messages);
    dup_audit::audit_tool_result_duplicates(messages, "sanitize.after_merge");
    fix_orphan_tool_results(messages);
    dup_audit::audit_tool_result_duplicates(messages, "sanitize.after_orphan");
    fix_unpaired_tool_uses(messages);
    dup_audit::audit_tool_result_duplicates(messages, "sanitize.after_unpaired");
    ensure_starts_with_user(messages);
    debug_assert_tool_pairing(messages);
}

/// Pass 1: Remove messages with no content blocks or only empty text.
fn remove_empty_messages(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        !msg.content.is_empty()
            && msg.content.iter().any(|block| match block {
                ContentBlock::Text { text } => !text.trim().is_empty(),
                _ => true,
            })
    });
}

/// Pass 2: Merge consecutive messages with the same role.
fn merge_consecutive_same_role(messages: &mut Vec<Message>) {
    if messages.len() < 2 {
        return;
    }

    let mut i = 0;
    while i + 1 < messages.len() {
        if messages[i].role == messages[i + 1].role {
            let next_content = messages[i + 1].content.clone();
            messages[i].content.extend(next_content);
            messages.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Pass 3: Remove orphan tool results that don't have a matching `tool_use`.
fn fix_orphan_tool_results(messages: &mut Vec<Message>) {
    let tool_use_ids: HashSet<String> = messages
        .iter()
        .flat_map(|msg| &msg.content)
        .filter_map(|block| {
            if let ContentBlock::ToolUse { id, .. } = block {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();

    for msg in messages.iter_mut() {
        msg.content.retain(|block| {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                tool_use_ids.contains(tool_use_id)
            } else {
                true
            }
        });
    }

    messages.retain(|msg| !msg.content.is_empty());
}

/// Pass 4: Ensure every assistant `tool_use` has a matching `tool_result`
/// in the **immediately following** user message (Anthropic positional rule).
///
/// Injects synthetic error results for any missing pairings.
fn fix_unpaired_tool_uses(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role != Role::Assistant {
            i += 1;
            continue;
        }

        let tool_use_ids: Vec<String> = messages[i]
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolUse { id, .. } = b {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        if tool_use_ids.is_empty() {
            i += 1;
            continue;
        }

        let existing_result_ids: HashSet<String> = messages
            .get(i + 1)
            .filter(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                            Some(tool_use_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let missing: Vec<String> = tool_use_ids
            .into_iter()
            .filter(|id| !existing_result_ids.contains(id))
            .collect();

        if !missing.is_empty() {
            let synthetic: Vec<ContentBlock> = missing
                .into_iter()
                .map(|id| {
                    ContentBlock::tool_result(
                        &id,
                        ToolResultContent::text("[Tool result was lost during context compaction]"),
                        true,
                    )
                })
                .collect();

            if i + 1 < messages.len() && messages[i + 1].role == Role::User {
                for (offset, block) in synthetic.into_iter().enumerate() {
                    messages[i + 1].content.insert(offset, block);
                }
            } else {
                messages.insert(i + 1, Message::new(Role::User, synthetic));
            }
        }

        i += 1;
    }
}

/// Pass 5: Ensure the conversation starts with a user message.
fn ensure_starts_with_user(messages: &mut Vec<Message>) {
    if messages.is_empty() || messages[0].role != Role::User {
        messages.insert(0, Message::user("[System: conversation context]"));
    }
}

/// Pass 6 (guard): Verify that every assistant message containing `tool_use`
/// is immediately followed by a user message containing matching `tool_result`
/// blocks.  Logs a warning on any violation so it surfaces in traces rather
/// than silently hitting the Anthropic 400 error.
fn debug_assert_tool_pairing(messages: &[Message]) {
    for (i, msg) in messages.iter().enumerate() {
        if msg.role != Role::Assistant {
            continue;
        }

        let tool_use_ids: Vec<&str> = msg
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolUse { id, .. } = b {
                    Some(id.as_str())
                } else {
                    None
                }
            })
            .collect();

        if tool_use_ids.is_empty() {
            continue;
        }

        let next_result_ids: HashSet<&str> = messages
            .get(i + 1)
            .filter(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                            Some(tool_use_id.as_str())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        for id in &tool_use_ids {
            if !next_result_ids.contains(id) {
                warn!(
                    message_index = i,
                    tool_use_id = id,
                    "Sanitization guard: tool_use without matching tool_result in next message"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_empty_messages() {
        let mut messages = vec![
            Message::user("hello"),
            Message::new(Role::User, vec![ContentBlock::text("")]),
            Message::assistant("world"),
        ];
        remove_empty_messages(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_merge_consecutive_same_role() {
        let mut messages = vec![
            Message::user("hello"),
            Message::user("world"),
            Message::assistant("hi"),
        ];
        merge_consecutive_same_role(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.len(), 2);
    }

    #[test]
    fn test_ensure_starts_with_user() {
        let mut messages = vec![Message::assistant("hi")];
        ensure_starts_with_user(&mut messages);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_fix_orphan_tool_results() {
        let mut messages = vec![
            Message::user("go"),
            Message::new(
                Role::User,
                vec![ContentBlock::tool_result(
                    "orphan_id",
                    ToolResultContent::text("result"),
                    false,
                )],
            ),
        ];
        fix_orphan_tool_results(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_validate_and_repair_full_pipeline() {
        let mut messages = vec![
            Message::assistant("oops first"),
            Message::user(""),
            Message::user("hello"),
            Message::user("world"),
        ];
        validate_and_repair(&mut messages);
        assert_eq!(messages[0].role, Role::User);
        assert!(messages.len() >= 2);
    }

    fn make_assistant_with_tool_use(tool_ids: &[&str]) -> Message {
        let mut blocks = vec![ContentBlock::text("I'll help")];
        for id in tool_ids {
            blocks.push(ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "a.rs"}),
            });
        }
        Message::new(Role::Assistant, blocks)
    }

    fn make_tool_result_msg(tool_ids: &[&str]) -> Message {
        let results: Vec<(String, ToolResultContent, bool)> = tool_ids
            .iter()
            .map(|id| (id.to_string(), ToolResultContent::text("ok"), false))
            .collect();
        Message::tool_results(results)
    }

    #[test]
    fn test_warning_between_tool_use_and_result_gets_merged() {
        let mut messages = vec![
            Message::user("do something"),
            make_assistant_with_tool_use(&["t1", "t2"]),
            Message::user("WARNING: blocked"),
            make_tool_result_msg(&["t1", "t2"]),
        ];
        validate_and_repair(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);

        let result_ids: HashSet<String> = messages[2]
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                    Some(tool_use_id.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(result_ids.contains("t1"));
        assert!(result_ids.contains("t2"));
    }

    #[test]
    fn test_tool_use_without_any_result_gets_synthetic() {
        let mut messages = vec![
            Message::user("do something"),
            make_assistant_with_tool_use(&["t1"]),
        ];
        validate_and_repair(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::User);

        let has_result = messages[2].content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"),
        );
        assert!(has_result, "should have synthetic tool_result for t1");
    }

    #[test]
    fn test_multiple_tool_uses_partial_results_get_synthetic() {
        let mut messages = vec![
            Message::user("task"),
            make_assistant_with_tool_use(&["t1", "t2", "t3"]),
            make_tool_result_msg(&["t1"]),
        ];
        validate_and_repair(&mut messages);

        let result_ids: HashSet<String> = messages[2]
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                    Some(tool_use_id.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(result_ids.contains("t1"));
        assert!(result_ids.contains("t2"), "synthetic result for t2");
        assert!(result_ids.contains("t3"), "synthetic result for t3");
    }

    #[test]
    fn test_build_warning_between_tool_use_and_result() {
        let mut messages = vec![
            Message::user("task"),
            make_assistant_with_tool_use(&["t1"]),
            Message::user("Build check failed with 3 error(s)"),
            Message::user("NOTE: first write checkpoint"),
            make_tool_result_msg(&["t1"]),
        ];
        validate_and_repair(&mut messages);

        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);

        let has_result = messages[2].content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"),
        );
        assert!(
            has_result,
            "tool_result should be in the merged user message"
        );
    }

    #[test]
    fn test_multi_turn_tool_use_all_paired() {
        let mut messages = vec![
            Message::user("task"),
            make_assistant_with_tool_use(&["t1"]),
            make_tool_result_msg(&["t1"]),
            make_assistant_with_tool_use(&["t2"]),
            make_tool_result_msg(&["t2"]),
        ];
        validate_and_repair(&mut messages);

        assert_eq!(messages.len(), 5);
        for i in [1, 3] {
            assert_eq!(messages[i].role, Role::Assistant);
            assert_eq!(messages[i + 1].role, Role::User);
        }
    }

    #[test]
    fn test_trailing_assistant_tool_use_gets_result() {
        let mut messages = vec![
            Message::user("task"),
            make_assistant_with_tool_use(&["t1"]),
            make_tool_result_msg(&["t1"]),
            make_assistant_with_tool_use(&["t2"]),
        ];
        validate_and_repair(&mut messages);

        assert!(messages.len() >= 5);
        let last_user = &messages[4];
        assert_eq!(last_user.role, Role::User);
        let has_t2 = last_user.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2"),
        );
        assert!(has_t2, "trailing tool_use should get synthetic result");
    }
}
