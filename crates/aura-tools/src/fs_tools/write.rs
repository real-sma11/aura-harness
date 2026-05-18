use crate::error::ToolError;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext};
use async_trait::async_trait;
use aura_core::ToolDefinition;
use aura_core::ToolResult;
use std::fs;
use std::path::Path;
use tracing::{debug, instrument};

/// Soft cap on `write_file` content size mirrored from
/// `aura_agent::constants::WRITE_FILE_CHUNK_BYTES`. Kept local to avoid a
/// circular dep between `aura-tools` and `aura-agent`; tests guard against
/// drift by asserting on the numeric threshold directly.
const WRITE_FILE_CHUNK_BYTES: usize = 12_000;

/// File extensions for which the unbalanced-brace/paren heuristic is
/// worth running. Anything outside this set (Markdown, plain text,
/// YAML/TOML, HTML, …) routinely contains unbalanced braces in
/// legitimate content (string literals, prose, closing tags) and
/// produced noisy warnings before this gate was added.
///
/// Intentionally narrow: the heuristic is a stopgap, not a parser, and
/// the cost of a false negative on a rare extension is much lower than
/// the cost of spamming a false positive on every README write.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "js", "ts", "tsx", "jsx", "c", "cpp", "h", "java", "go", "cs", "swift", "kt",
];

fn is_elided_write_placeholder(content: &str) -> bool {
    content.starts_with("<<<AURA_ELIDED_CONTENT::") && content.ends_with(">>>")
}

fn has_redacted_field_marker(args: &serde_json::Value, field: &str) -> bool {
    let Some(marker) = args.get("_redacted").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if marker
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind == "aura_compaction_redaction")
        && marker
            .get("field")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|marked| marked == field)
    {
        return true;
    }
    marker
        .get("fields")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|fields| {
            fields.iter().any(|entry| {
                entry
                    .get("field")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|marked| marked == field)
            })
        })
}

fn is_code_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            CODE_EXTENSIONS
                .iter()
                .any(|code_ext| ext.eq_ignore_ascii_case(code_ext))
        })
}

/// Check whether `content` has unbalanced `{}`/`()` pairs, which may
/// indicate truncated output from an LLM.
///
/// Only runs the balance check for files whose extension appears in
/// [`CODE_EXTENSIONS`] — Markdown, prose, and config files routinely
/// carry unbalanced braces inside string literals / code fences and
/// shouldn't trip the warning.
fn looks_truncated(path: &Path, content: &str) -> bool {
    if !is_code_file(path) {
        return false;
    }
    let mut brace_depth: i64 = 0;
    let mut paren_depth: i64 = 0;
    for ch in content.chars() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            _ => {}
        }
    }
    brace_depth != 0 || paren_depth != 0
}

/// Write content to a file.
///
/// Parent directories are always created automatically (matching aura-app
/// behaviour). The `create_dirs` parameter is kept for backward compatibility
/// but effectively defaults to `true`.
///
/// Safety heuristics:
/// - Rejects writes that would replace an existing file with content < 10%
///   of the original size.
/// - Warns (via metadata) when the content has unbalanced braces/parens.
/// - Performs post-write verification of byte count.
#[instrument(skip(sandbox, content), fields(path = %path))]
pub fn fs_write(
    sandbox: &Sandbox,
    path: &str,
    content: &str,
    create_dirs: bool,
) -> Result<ToolResult, ToolError> {
    if is_elided_write_placeholder(content) {
        return Err(ToolError::InvalidArguments(
            "content is an elided history placeholder; supply the real file content. \
             Prior turns redact write_file/edit_file inputs to save context; never copy \
             the placeholder verbatim. Re-emit the full intended content here."
                .to_string(),
        ));
    }

    let _ = create_dirs; // kept for API compat; always creates dirs
    let resolved = sandbox.resolve_new(path)?;
    debug!(?resolved, "Writing file");

    let file_existed = resolved.exists();
    // Snapshot pre-content before the write so we can compute a real
    // line-level diff rather than guessing from sizes alone. Only
    // captured when the file already exists; for a fresh create the
    // pre-content is the empty string (no allocation needed). We read
    // it lazily into a String so a non-UTF-8 file silently falls back
    // to "no pre-content" (lines_added counts the new content as a
    // pure insert) rather than failing the write — line counting is a
    // best-effort observability signal, not a correctness gate.
    let pre_content = if file_existed {
        fs::read_to_string(&resolved).unwrap_or_default()
    } else {
        String::new()
    };
    let existing_size = if file_existed {
        usize::try_from(fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0)).unwrap_or(usize::MAX)
    } else {
        0
    };

    // Truncation heuristic: reject if new content < 10% of existing file
    if file_existed && existing_size > 0 && content.len() < existing_size / 10 {
        return Err(ToolError::InvalidArguments(
            "New content is less than 10% of existing file size. \
             This likely indicates truncated output."
                .to_string(),
        ));
    }

    // Always create parent directories
    if let Some(parent) = resolved.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| {
                ToolError::Io(std::io::Error::new(
                    e.kind(),
                    format!("create_dir_all({}): {e}", parent.display()),
                ))
            })?;
        }
    }

    fs::write(&resolved, content).map_err(|e| {
        ToolError::Io(std::io::Error::new(
            e.kind(),
            format!("write({}): {e}", resolved.display()),
        ))
    })?;

    // Post-write verification
    let on_disk_size = usize::try_from(fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0))
        .unwrap_or(usize::MAX);
    if on_disk_size != content.len() {
        return Err(ToolError::InvalidArguments(format!(
            "Post-write verification failed: wrote {} bytes but file is {} bytes on disk",
            content.len(),
            on_disk_size
        )));
    }

    let bytes_written = content.len();
    let truncated_warning = looks_truncated(&resolved, content);

    // Compute the actual line diff between pre-content and new content.
    // For a fresh create, pre_content is empty so this collapses to
    // "lines_added = new line count, lines_removed = 0".
    let (lines_added, lines_removed) = super::diff::count_line_diff(&pre_content, content);

    let mut result = ToolResult::success(
        "write_file",
        format!("Wrote {bytes_written} bytes to {path}"),
    )
    .with_metadata("bytes_written", bytes_written.to_string())
    .with_metadata("file_existed", file_existed.to_string())
    .with_line_diff(lines_added, lines_removed);

    if truncated_warning {
        result = result.with_metadata(
            "warning",
            "Content has unbalanced braces/parentheses – may be truncated".to_string(),
        );
    }

    if bytes_written > WRITE_FILE_CHUNK_BYTES {
        result = result.with_metadata("chunk_suggestion", "true");
    }

    Ok(result)
}

/// `fs_write` tool: write content to a file.
pub struct FsWriteTool;

#[async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description:
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative to workspace root)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    },
                    "create_dirs": {
                        "type": "boolean",
                        "description": "Create parent directories if they don't exist (default: true)"
                    }
                },
                "required": ["path", "content"]
            }),
            cache_control: None,
            // Stream the `content` string live as the model writes it so the
            // UI's file preview fills in character-by-character instead of
            // waiting for the full tool-use block to close.
            eager_input_streaming: Some(true),
        }
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        if has_redacted_field_marker(&args, "content") {
            return Err(ToolError::InvalidArguments(
                "content is an elided history placeholder; supply the real file content. \
                 Prior turns redact write_file/edit_file inputs to save context; never copy \
                 the placeholder verbatim. Re-emit the full intended content here."
                    .to_string(),
            ));
        }
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path' argument".into()))?
            .to_string();
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'content' argument".into()))?
            .to_string();
        let create_dirs = args["create_dirs"].as_bool().unwrap_or(true);
        let sandbox = ctx.sandbox.clone();
        super::spawn_blocking_tool(move || fs_write(&sandbox, &path, &content, create_dirs)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_sandbox() -> (Sandbox, TempDir) {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (sandbox, dir)
    }

    #[test]
    fn test_fs_write_new_file() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "new.txt", "Hello, world!", false).unwrap();
        assert!(result.ok);

        let content = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "Hello, world!");
    }

    #[test]
    fn test_fs_write_rejects_elided_history_placeholder() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(
            &sandbox,
            "placeholder.txt",
            "<<<AURA_ELIDED_CONTENT::42_bytes>>>",
            false,
        );

        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
        if let Err(ToolError::InvalidArguments(msg)) = result {
            assert!(msg.contains("elided history placeholder"));
            assert!(msg.contains("supply the real file content"));
        }
        assert!(!dir.path().join("placeholder.txt").exists());
    }

    #[test]
    fn test_write_detector_rejects_structured_redaction_marker() {
        let args = serde_json::json!({
            "path": "placeholder.txt",
            "_redacted": {
                "kind": "aura_compaction_redaction",
                "version": 1,
                "field": "content",
                "bytes": 42
            }
        });

        assert!(has_redacted_field_marker(&args, "content"));
    }

    #[test]
    fn test_fs_write_overwrite_file() {
        let (sandbox, dir) = create_test_sandbox();

        fs::write(dir.path().join("existing.txt"), "old content").unwrap();

        let result = fs_write(&sandbox, "existing.txt", "new content", false).unwrap();
        assert!(result.ok);

        let content = fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new content");
    }

    #[test]
    fn test_fs_write_create_dirs() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "nested/deep/file.txt", "content", true).unwrap();
        assert!(result.ok);

        assert!(dir.path().join("nested/deep/file.txt").exists());
        let content = fs::read_to_string(dir.path().join("nested/deep/file.txt")).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn test_fs_write_creates_parent_dirs_by_default() {
        let (sandbox, dir) = create_test_sandbox();

        // Even with create_dirs=false, parent dirs are now always created
        let result = fs_write(&sandbox, "auto/created/file.txt", "content", false).unwrap();
        assert!(result.ok);
        assert!(dir.path().join("auto/created/file.txt").exists());
    }

    #[test]
    fn test_fs_write_truncation_heuristic_rejects_small() {
        let (sandbox, dir) = create_test_sandbox();

        // Write a large file first
        let large = "x".repeat(1000);
        fs::write(dir.path().join("big.txt"), &large).unwrap();

        // Attempt to overwrite with tiny content (< 10%)
        let result = fs_write(&sandbox, "big.txt", "tiny", false);
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
        if let Err(ToolError::InvalidArguments(msg)) = result {
            assert!(msg.contains("10%"));
        }
    }

    #[test]
    fn test_fs_write_post_write_verification() {
        let (sandbox, _dir) = create_test_sandbox();

        let content = "verified content";
        let result = fs_write(&sandbox, "verify.txt", content, false).unwrap();
        assert!(result.ok);
        assert_eq!(
            result.metadata.get("bytes_written").unwrap(),
            &content.len().to_string()
        );
    }

    #[test]
    fn test_fs_write_bytes_written_metadata() {
        let (sandbox, _dir) = create_test_sandbox();

        let content = "12345";
        let result = fs_write(&sandbox, "counted.txt", content, false).unwrap();

        assert_eq!(result.metadata.get("bytes_written").unwrap(), "5");
    }

    #[test]
    fn test_fs_write_unicode_content() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "Hello 世界! 🌍 Привет";
        let result = fs_write(&sandbox, "unicode.txt", content, false).unwrap();
        assert!(result.ok);

        let read_back = fs::read_to_string(dir.path().join("unicode.txt")).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn test_fs_write_large_file_over_1mb() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "x".repeat(1_100_000);
        let result = fs_write(&sandbox, "large.bin", &content, false).unwrap();
        assert!(result.ok);

        let on_disk = fs::read_to_string(dir.path().join("large.bin")).unwrap();
        assert_eq!(on_disk.len(), 1_100_000);
    }

    #[test]
    fn test_fs_write_special_chars_in_filename() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "file with spaces.txt", "data", false).unwrap();
        assert!(result.ok);
        assert!(dir.path().join("file with spaces.txt").exists());
    }

    #[test]
    fn test_fs_write_empty_content() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "empty.txt", "", false).unwrap();
        assert!(result.ok);

        let content = fs::read_to_string(dir.path().join("empty.txt")).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_fs_write_overwrite_with_exactly_10_percent() {
        let (sandbox, dir) = create_test_sandbox();

        let large = "x".repeat(1000);
        fs::write(dir.path().join("boundary.txt"), &large).unwrap();

        // 100 bytes = exactly 10% of 1000 — should be accepted
        let small = "y".repeat(100);
        let result = fs_write(&sandbox, "boundary.txt", &small, false).unwrap();
        assert!(result.ok);
    }

    #[test]
    fn test_fs_write_overwrite_with_just_under_10_percent() {
        let (sandbox, dir) = create_test_sandbox();

        let large = "x".repeat(1000);
        fs::write(dir.path().join("boundary2.txt"), &large).unwrap();

        // 99 bytes = just under 10% of 1000 — should be rejected
        let small = "y".repeat(99);
        let result = fs_write(&sandbox, "boundary2.txt", &small, false);
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn test_fs_write_truncation_warning_unbalanced_braces_in_rust() {
        let (sandbox, _dir) = create_test_sandbox();

        // `.rs` is code-like — unbalanced braces should trip the
        // warning.
        let content = "fn main() { if true {";
        let result = fs_write(&sandbox, "warn.rs", content, false).unwrap();
        assert!(result.ok);
        assert!(result.metadata.contains_key("warning"));
    }

    #[test]
    fn test_fs_write_no_truncation_warning_balanced() {
        let (sandbox, _dir) = create_test_sandbox();

        let content = "fn main() { println!(); }";
        let result = fs_write(&sandbox, "ok.rs", content, false).unwrap();
        assert!(result.ok);
        assert!(!result.metadata.contains_key("warning"));
    }

    #[test]
    fn test_fs_write_no_truncation_warning_on_markdown() {
        // Markdown routinely has unbalanced braces inside string
        // literals, code fences, or prose (e.g. `Vec<T> { ... }` in a
        // doc). The heuristic must not fire for `.md` files.
        let (sandbox, _dir) = create_test_sandbox();

        let content = "# Note\n\nThe struct is shaped like `Foo { a, b`.\n";
        let result = fs_write(&sandbox, "notes.md", content, false).unwrap();
        assert!(result.ok);
        assert!(
            !result.metadata.contains_key("warning"),
            "markdown must not trip the truncation heuristic"
        );
    }

    #[test]
    fn test_fs_write_no_truncation_warning_on_txt() {
        // Plain-text files are not code — suppress warnings even if
        // braces happen to be unbalanced.
        let (sandbox, _dir) = create_test_sandbox();

        let content = "todo: check the { state";
        let result = fs_write(&sandbox, "notes.txt", content, false).unwrap();
        assert!(result.ok);
        assert!(!result.metadata.contains_key("warning"));
    }

    #[test]
    fn test_fs_write_file_existed_metadata() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "new_file.txt", "initial", false).unwrap();
        assert_eq!(result.metadata.get("file_existed").unwrap(), "false");

        fs::write(dir.path().join("old_file.txt"), "original").unwrap();
        let result = fs_write(&sandbox, "old_file.txt", "replaced!", false).unwrap();
        assert_eq!(result.metadata.get("file_existed").unwrap(), "true");
    }

    #[test]
    fn test_fs_write_deeply_nested_path() {
        let (sandbox, dir) = create_test_sandbox();

        let result = fs_write(&sandbox, "a/b/c/d/e/f/deep.txt", "deep", true).unwrap();
        assert!(result.ok);
        assert!(dir.path().join("a/b/c/d/e/f/deep.txt").exists());
    }

    #[test]
    fn fs_write_metadata_chunk_suggestion_when_over_threshold() {
        let (sandbox, _dir) = create_test_sandbox();

        let big = "x".repeat(13_000);
        let result = fs_write(&sandbox, "big_chunk.txt", &big, false).unwrap();
        assert!(result.ok);
        assert_eq!(
            result.metadata.get("chunk_suggestion").map(String::as_str),
            Some("true"),
            "chunk_suggestion metadata should be set when content exceeds WRITE_FILE_CHUNK_BYTES"
        );

        let small = "y".repeat(2_000);
        let result = fs_write(&sandbox, "small_chunk.txt", &small, false).unwrap();
        assert!(result.ok);
        assert!(
            !result.metadata.contains_key("chunk_suggestion"),
            "chunk_suggestion metadata should be absent for content under WRITE_FILE_CHUNK_BYTES"
        );
    }

    // ====================================================================
    // line_diff coverage — verifies fs_write attaches accurate line counts
    // for both the create-from-scratch and overwrite flows so the agent
    // loop can populate FileChange.lines_added/_removed without re-reading
    // the filesystem.
    // ====================================================================

    #[test]
    fn fs_write_create_reports_added_lines_only() {
        let (sandbox, _dir) = create_test_sandbox();
        let result = fs_write(&sandbox, "new.rs", "fn a() {}\nfn b() {}\n", false).unwrap();
        let line_diff = result.line_diff.expect("create should report a line diff");
        assert_eq!(line_diff.lines_added, 2);
        assert_eq!(line_diff.lines_removed, 0);
    }

    #[test]
    fn fs_write_overwrite_reports_replacement_diff() {
        let (sandbox, dir) = create_test_sandbox();
        // Pre-content: 3 lines.
        fs::write(dir.path().join("lib.rs"), "old1\nold2\nold3\n").unwrap();
        // Overwrite with 2 different lines plus 1 unchanged tail —
        // 2 inserts, 2 deletes (same-position lines that differ become
        // a paired insert+delete in similar's line model).
        let result = fs_write(&sandbox, "lib.rs", "new1\nnew2\nold3\n", false).unwrap();
        let line_diff = result
            .line_diff
            .expect("overwrite should report a line diff");
        assert_eq!(line_diff.lines_added, 2);
        assert_eq!(line_diff.lines_removed, 2);
    }

    #[test]
    fn fs_write_overwrite_with_identical_content_reports_zero_diff() {
        let (sandbox, dir) = create_test_sandbox();
        fs::write(dir.path().join("noop.rs"), "same\nsame\n").unwrap();
        let result = fs_write(&sandbox, "noop.rs", "same\nsame\n", false).unwrap();
        let line_diff = result
            .line_diff
            .expect("identical overwrite still reports a diff (just zero)");
        assert_eq!(line_diff.lines_added, 0);
        assert_eq!(line_diff.lines_removed, 0);
    }
}
