use super::*;
use tempfile::TempDir;

fn create_test_sandbox() -> (Sandbox, TempDir) {
    let dir = TempDir::new().unwrap();
    let sandbox = Sandbox::new(dir.path()).unwrap();
    (sandbox, dir)
}

#[test]
fn test_fs_edit_single_replacement() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "Hello, World!").unwrap();

    let result = fs_edit(&sandbox, "edit.txt", "World", "Aura", false).unwrap();
    assert!(result.ok);
    assert_eq!(result.metadata.get("replacements").unwrap(), "1");

    let content = fs::read_to_string(dir.path().join("edit.txt")).unwrap();
    assert_eq!(content, "Hello, Aura!");
}

#[test]
fn test_fs_edit_rejects_elided_old_text_placeholder() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "Hello, World!").unwrap();

    let result = fs_edit(
        &sandbox,
        "edit.txt",
        "<<<AURA_ELIDED_OLD::13_chars>>>",
        "Aura",
        false,
    );

    assert!(matches!(result, Err(ToolError::CompactionStructural(_))));
    if let Err(ToolError::CompactionStructural(msg)) = result {
        assert!(msg.contains("old_text is an elided history placeholder"));
        assert!(msg.contains("supply the real edit text"));
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("edit.txt")).unwrap(),
        "Hello, World!"
    );
}

#[test]
fn test_fs_edit_rejects_elided_new_text_placeholder() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "Hello, World!").unwrap();

    let result = fs_edit(
        &sandbox,
        "edit.txt",
        "World",
        "<<<AURA_ELIDED_NEW::4_chars>>>",
        false,
    );

    assert!(matches!(result, Err(ToolError::CompactionStructural(_))));
    if let Err(ToolError::CompactionStructural(msg)) = result {
        assert!(msg.contains("new_text is an elided history placeholder"));
        assert!(msg.contains("supply the real edit text"));
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("edit.txt")).unwrap(),
        "Hello, World!"
    );
}

#[test]
fn test_edit_detector_rejects_structured_redaction_marker() {
    let args = serde_json::json!({
        "path": "edit.txt",
        "_redacted": {
            "kind": "aura_compaction_redaction",
            "version": 1,
            "fields": [
                { "field": "old_text", "bytes": 13 },
                { "field": "new_text", "bytes": 4 }
            ]
        }
    });

    assert!(has_redacted_field_marker(&args, "old_text"));
    assert!(has_redacted_field_marker(&args, "new_text"));
}

#[tokio::test]
async fn test_fs_edit_redaction_marker_error_is_structural() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(dir.path().join("edit.txt"), "Hello, World!").unwrap();
    let ctx = ToolContext::new(sandbox, crate::ToolConfig::default());
    let args = serde_json::json!({
        "path": "edit.txt",
        "_redacted": {
            "kind": "aura_compaction_redaction",
            "version": 1,
            "fields": [
                { "field": "old_text", "bytes": 13 },
                { "field": "new_text", "bytes": 4 }
            ]
        }
    });

    let result = FsEditTool.execute(&ctx, args).await;

    assert!(matches!(result, Err(ToolError::CompactionStructural(_))));
}

#[test]
fn test_fs_edit_replace_all() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "foo bar foo baz foo").unwrap();

    let result = fs_edit(&sandbox, "edit.txt", "foo", "qux", true).unwrap();
    assert!(result.ok);
    assert_eq!(result.metadata.get("replacements").unwrap(), "3");

    let content = fs::read_to_string(dir.path().join("edit.txt")).unwrap();
    assert_eq!(content, "qux bar qux baz qux");
}

#[test]
fn test_fs_edit_multi_match_without_replace_all_errors() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "foo bar foo baz foo").unwrap();

    let result = fs_edit(&sandbox, "edit.txt", "foo", "qux", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    if let Err(ToolError::InvalidArguments(msg)) = result {
        assert!(msg.contains("3 occurrences"));
        assert!(msg.contains("replace_all=true"));
    }
}

#[test]
fn test_fs_edit_text_not_found() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("edit.txt"), "Hello, World!").unwrap();

    let result = fs_edit(&sandbox, "edit.txt", "NotFound", "Replacement", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_fs_edit_not_a_file() {
    let (sandbox, dir) = create_test_sandbox();

    fs::create_dir(dir.path().join("dir")).unwrap();

    let result = fs_edit(&sandbox, "dir", "old", "new", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_fs_edit_multiline() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "line1\nold_content\nline3";
    fs::write(dir.path().join("multi.txt"), content).unwrap();

    let result = fs_edit(&sandbox, "multi.txt", "old_content", "new_content", false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("multi.txt")).unwrap();
    assert_eq!(updated, "line1\nnew_content\nline3");
}

#[test]
fn test_fs_edit_fuzzy_match_whitespace_difference() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
    fs::write(dir.path().join("fuzzy.rs"), content).unwrap();

    let old_text = "let x = 1;\nlet y = 2;";
    let new_text = "let x = 10;\nlet y = 20;";

    let result = fs_edit(&sandbox, "fuzzy.rs", old_text, new_text, false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("fuzzy.rs")).unwrap();
    assert!(updated.contains("let x = 10;"));
    assert!(updated.contains("let y = 20;"));
}

#[test]
fn test_fs_edit_fuzzy_match_no_match() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("nope.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let result = fs_edit(&sandbox, "nope.txt", "totally\ndifferent", "new", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    if let Err(ToolError::InvalidArguments(msg)) = result {
        assert!(msg.contains("not found"));
    }
}

#[test]
fn test_fs_edit_shrinkage_guard_rejects_large_reduction() {
    let (sandbox, dir) = create_test_sandbox();

    let big_content = "a\n".repeat(500);
    fs::write(dir.path().join("shrink.txt"), &big_content).unwrap();

    let result = fs_edit(&sandbox, "shrink.txt", &big_content, "x", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    if let Err(ToolError::InvalidArguments(msg)) = result {
        assert!(msg.contains("20%"));
    }
}

#[test]
fn test_fs_edit_shrinkage_guard_allows_normal_edit() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "Hello, World! This is a test file with enough content.";
    fs::write(dir.path().join("normal.txt"), content).unwrap();

    let result = fs_edit(&sandbox, "normal.txt", "World", "Aura", false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("normal.txt")).unwrap();
    assert_eq!(
        updated,
        "Hello, Aura! This is a test file with enough content."
    );
}

#[test]
fn test_fs_edit_crlf_normalization() {
    let (sandbox, dir) = create_test_sandbox();

    let crlf_content = "line1\r\nline2\r\nline3\r\n";
    fs::write(dir.path().join("crlf.txt"), crlf_content).unwrap();

    let result = fs_edit(&sandbox, "crlf.txt", "line2", "replaced", false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("crlf.txt")).unwrap();
    assert!(updated.contains("\r\n"));
    assert!(updated.contains("replaced"));
    assert!(!updated.contains("line2"));
}

#[test]
fn test_fs_edit_empty_file_text_not_found() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("empty.txt"), "").unwrap();

    let result = fs_edit(&sandbox, "empty.txt", "anything", "new", false);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_fs_edit_old_text_with_regex_special_chars() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "value = arr[0].map(|x| x + 1);";
    fs::write(dir.path().join("regex_chars.rs"), content).unwrap();

    let result = fs_edit(
        &sandbox,
        "regex_chars.rs",
        "arr[0].map(|x| x + 1)",
        "arr[0].filter(|x| x > 0)",
        false,
    )
    .unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("regex_chars.rs")).unwrap();
    assert_eq!(updated, "value = arr[0].filter(|x| x > 0);");
}

#[test]
fn test_fs_edit_old_text_with_parentheses_and_braces() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "fn foo() { bar(baz{}) }";
    fs::write(dir.path().join("parens.txt"), content).unwrap();

    let result = fs_edit(&sandbox, "parens.txt", "bar(baz{})", "qux()", false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("parens.txt")).unwrap();
    assert_eq!(updated, "fn foo() { qux() }");
}

#[test]
fn test_fs_edit_nonexistent_file() {
    let (sandbox, _dir) = create_test_sandbox();

    let result = fs_edit(&sandbox, "nope.txt", "old", "new", false);
    assert!(matches!(result, Err(ToolError::PathNotFound(_))));
}

#[test]
fn test_fs_edit_replace_all_zero_occurrences() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("none.txt"), "hello world").unwrap();

    let result = fs_edit(&sandbox, "none.txt", "zzz", "xxx", true);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_fs_edit_single_char_replacement() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("char.txt"), "a+b=c").unwrap();

    let result = fs_edit(&sandbox, "char.txt", "+", "-", false).unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("char.txt")).unwrap();
    assert_eq!(updated, "a-b=c");
}

#[test]
fn test_fs_edit_multiline_replacement_preserves_context() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "header\nfn old() {\n    body();\n}\nfooter\n";
    fs::write(dir.path().join("multi.rs"), content).unwrap();

    let result = fs_edit(
        &sandbox,
        "multi.rs",
        "fn old() {\n    body();\n}",
        "fn new() {\n    new_body();\n}",
        false,
    )
    .unwrap();
    assert!(result.ok);

    let updated = fs::read_to_string(dir.path().join("multi.rs")).unwrap();
    assert!(updated.starts_with("header\n"));
    assert!(updated.ends_with("footer\n"));
    assert!(updated.contains("fn new()"));
}

// ====================================================================
// line_diff coverage — verifies fs_edit attaches line counts computed
// over the *whole-file* pre/post pair (not just the substring args), so
// the agent loop sees the same numbers a `git diff --numstat` would.
// ====================================================================

#[test]
fn fs_edit_single_line_replacement_reports_one_in_one_out() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(
        dir.path().join("lib.rs"),
        "fn header() {}\nfn old() {}\nfn footer() {}\n",
    )
    .unwrap();

    let result = fs_edit(&sandbox, "lib.rs", "fn old() {}", "fn new() {}", false).unwrap();
    let line_diff = result.line_diff.expect("edit always reports a line diff");
    assert_eq!(line_diff.lines_added, 1);
    assert_eq!(line_diff.lines_removed, 1);
}

#[test]
fn fs_edit_multi_line_expansion_reports_correct_diff() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(
        dir.path().join("lib.rs"),
        "header\nfn old() {\n    body();\n}\nfooter\n",
    )
    .unwrap();

    // 3-line block -> 4-line block (added one line, replaced one).
    let result = fs_edit(
        &sandbox,
        "lib.rs",
        "fn old() {\n    body();\n}",
        "fn new() {\n    new_body();\n    extra();\n}",
        false,
    )
    .unwrap();
    let line_diff = result.line_diff.expect("edit always reports a line diff");
    // body() -> new_body() is one swap; extra() is a pure insert; the
    // `fn old()` -> `fn new()` swap is another paired insert/delete.
    assert_eq!(line_diff.lines_added, 3);
    assert_eq!(line_diff.lines_removed, 2);
}

#[test]
fn fs_edit_replace_all_reports_aggregated_diff() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(dir.path().join("lib.rs"), "x = 1\nx = 2\nx = 3\n").unwrap();

    let result = fs_edit(&sandbox, "lib.rs", "x", "y", true).unwrap();
    let line_diff = result.line_diff.expect("edit always reports a line diff");
    // All three lines change: 3 inserts + 3 deletes.
    assert_eq!(line_diff.lines_added, 3);
    assert_eq!(line_diff.lines_removed, 3);
}
