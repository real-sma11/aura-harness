//! Helper functions for the agent loop.
//!
//! Phase 6a moved `append_warning`, `is_exploration_tool`, and
//! `is_write_tool` into `aura-agent-steering` so the steering crate
//! does not need to depend back on `aura-agent`. This module
//! re-exports them under their historical `crate::helpers::*`
//! paths so every existing call site keeps working unchanged.

use aura_core::LineDiff;
use std::path::Path;

use crate::types::{FileChange, FileChangeKind};

pub use aura_agent_steering::{append_warning, is_exploration_tool, is_write_tool};

#[cfg(test)]
use aura_reasoner::{Message, Role};

/// Infer file mutations for a successful write tool call.
///
/// `lines_added` / `lines_removed` are populated from `line_diff` when
/// the tool layer attached one (the `fs_write` / `fs_edit` /
/// `fs_delete` tools all do; see
/// [`aura_core::ToolResult::with_line_diff`]). When `line_diff` is
/// `None` — e.g. a custom tool that mutates files but doesn't compute
/// counts — both fields default to 0 and the dashboard treats it as
/// "unknown".
#[must_use]
pub fn infer_file_changes(
    tool_name: &str,
    input: &serde_json::Value,
    base_path: Option<&Path>,
    line_diff: Option<&LineDiff>,
) -> Vec<FileChange> {
    let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
        return Vec::new();
    };

    let existed_before = base_path.map(|base| base.join(path).exists());
    let kind = match tool_name {
        "write_file" => {
            if matches!(existed_before, Some(false)) {
                FileChangeKind::Create
            } else {
                FileChangeKind::Modify
            }
        }
        "edit_file" => FileChangeKind::Modify,
        "delete_file" => {
            if matches!(existed_before, Some(false)) {
                return Vec::new();
            }
            FileChangeKind::Delete
        }
        _ => return Vec::new(),
    };

    let (lines_added, lines_removed) =
        line_diff.map_or((0, 0), |d| (d.lines_added, d.lines_removed));

    vec![FileChange {
        path: path.to_string(),
        kind,
        lines_added,
        lines_removed,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_append_warning_to_existing_user_message() {
        let mut messages = vec![Message::user("hello")];
        append_warning(&mut messages, "WARNING: something");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.len(), 2);
    }

    #[test]
    fn test_append_warning_after_assistant() {
        let mut messages = vec![Message::assistant("response")];
        append_warning(&mut messages, "WARNING: something");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::User);
    }

    #[test]
    fn test_infer_file_changes_write_create_without_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let input = serde_json::json!({"path": "src/new.rs"});
        let changes = infer_file_changes("write_file", &input, Some(dir.path()), None);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/new.rs");
        assert!(matches!(changes[0].kind, FileChangeKind::Create));
    }

    #[test]
    fn test_infer_file_changes_write_modify_with_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "old").unwrap();

        let input = serde_json::json!({"path": "src/lib.rs"});
        let changes = infer_file_changes("write_file", &input, Some(dir.path()), None);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].kind, FileChangeKind::Modify));
    }

    #[test]
    fn test_infer_file_changes_write_defaults_to_modify_without_base_path() {
        let input = serde_json::json!({"path": "src/lib.rs"});
        let changes = infer_file_changes("write_file", &input, None, None);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].kind, FileChangeKind::Modify));
    }

    // ====================================================================
    // line_diff plumbing — verifies infer_file_changes promotes the
    // tool-level LineDiff hint into FileChange.lines_added/_removed.
    // ====================================================================

    #[test]
    fn infer_file_changes_uses_line_diff_hint_for_edit() {
        let input = serde_json::json!({"path": "src/lib.rs"});
        let hint = LineDiff {
            lines_added: 7,
            lines_removed: 2,
        };
        let changes = infer_file_changes("edit_file", &input, None, Some(&hint));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].lines_added, 7);
        assert_eq!(changes[0].lines_removed, 2);
    }

    #[test]
    fn infer_file_changes_uses_line_diff_hint_for_write() {
        let input = serde_json::json!({"path": "src/new.rs"});
        let hint = LineDiff {
            lines_added: 50,
            lines_removed: 0,
        };
        let changes = infer_file_changes("write_file", &input, None, Some(&hint));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].lines_added, 50);
        assert_eq!(changes[0].lines_removed, 0);
    }

    #[test]
    fn infer_file_changes_uses_line_diff_hint_for_delete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/old.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "doomed").unwrap();
        let input = serde_json::json!({"path": "src/old.rs"});
        let hint = LineDiff {
            lines_added: 0,
            lines_removed: 30,
        };
        let changes = infer_file_changes("delete_file", &input, Some(dir.path()), Some(&hint));
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].kind, FileChangeKind::Delete));
        assert_eq!(changes[0].lines_added, 0);
        assert_eq!(changes[0].lines_removed, 30);
    }

    #[test]
    fn infer_file_changes_defaults_to_zero_when_hint_absent() {
        // Tool-layer didn't compute a diff (e.g. a custom file mutator);
        // counts default to 0 ("unknown") rather than fabricating a value.
        let input = serde_json::json!({"path": "src/lib.rs"});
        let changes = infer_file_changes("edit_file", &input, None, None);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].lines_added, 0);
        assert_eq!(changes[0].lines_removed, 0);
    }
}
