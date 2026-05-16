use crate::error::ToolError;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext};
use async_trait::async_trait;
use aura_core::ToolDefinition;
use aura_core::ToolResult;
use std::fs;
use std::io::Read;
use tracing::{debug, instrument};

/// Build the standard truncation marker used by `fs_read` so the LLM gets a
/// consistent, machine-readable hint about how to recover the missing bytes.
fn truncation_marker(dropped: usize, total: usize) -> String {
    format!(
        "\n... [truncated {dropped} of {total} bytes; use start_line/end_line for slicing or pass max_bytes to read more.]"
    )
}

/// Read file contents, optionally restricted to a line range.
///
/// When `start_line` / `end_line` are provided (1-indexed, inclusive), only
/// the requested slice of lines is returned, prefixed with line numbers.
/// This avoids dumping entire large files into the context window.
#[instrument(skip(sandbox), fields(path = %path, max_bytes))]
pub fn fs_read(
    sandbox: &Sandbox,
    path: &str,
    max_bytes: usize,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<ToolResult, ToolError> {
    let resolved = sandbox.resolve_existing(path)?;
    debug!(?resolved, "Reading file");

    if !resolved.is_file() {
        return Err(ToolError::InvalidArguments(format!("{path} is not a file")));
    }

    let metadata = fs::metadata(&resolved).map_err(|e| {
        ToolError::Io(std::io::Error::new(
            e.kind(),
            format!("metadata({}): {e}", resolved.display()),
        ))
    })?;
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);

    let truncated = size > max_bytes;
    let read_len = size.min(max_bytes);
    let file = fs::File::open(&resolved).map_err(|e| {
        ToolError::Io(std::io::Error::new(
            e.kind(),
            format!("open({}): {e}", resolved.display()),
        ))
    })?;
    let mut contents = Vec::with_capacity(read_len);
    file.take(read_len as u64)
        .read_to_end(&mut contents)
        .map_err(|e| {
            ToolError::Io(std::io::Error::new(
                e.kind(),
                format!("read({}): {e}", resolved.display()),
            ))
        })?;
    if truncated {
        contents.extend_from_slice(truncation_marker(size - read_len, size).as_bytes());
    }

    if start_line.is_some() || end_line.is_some() {
        let text = String::from_utf8_lossy(&contents);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = start_line.unwrap_or(1).max(1);
        let end = end_line.unwrap_or(total).min(total);

        if start > total {
            return Ok(ToolResult::success(
                "read_file",
                format!("(file has {total} lines, requested start_line={start})"),
            )
            .with_metadata("total_lines", total.to_string()));
        }

        let sliced: Vec<String> = lines[(start - 1)..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}|{}", start + i, line))
            .collect();
        let mut output = sliced.join("\n");
        let mut output_truncated = false;
        if output.len() > max_bytes {
            let original_len = output.len();
            let mut idx = max_bytes;
            while idx > 0 && !output.is_char_boundary(idx) {
                idx -= 1;
            }
            output.truncate(idx);
            output.push_str(&truncation_marker(original_len - idx, original_len));
            output_truncated = true;
        }
        let mut result = ToolResult::success("read_file", output)
            .with_metadata("size", size.to_string())
            .with_metadata("total_lines", total.to_string())
            .with_metadata("start_line", start.to_string())
            .with_metadata("end_line", end.to_string());
        if truncated || output_truncated {
            result = result.with_metadata("truncated", "true");
        }
        Ok(result)
    } else {
        let mut result = ToolResult::success("read_file", contents)
            .with_metadata("size", size.to_string());
        if truncated {
            result = result.with_metadata("truncated", "true");
        }
        Ok(result)
    }
}

/// `fs_read` tool: read file contents.
pub struct FsReadTool;

#[async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a file's contents. Default returns up to 64 KB; larger files are truncated with a clear marker. Prefer start_line/end_line for slicing big files; only set max_bytes explicitly when a single call must exceed 64 KB.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read (relative to workspace root)"
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Maximum bytes to read (default: 1MB). Useful for large files."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "First line to return (1-indexed, inclusive). Omit to start from the beginning."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Last line to return (1-indexed, inclusive). Omit to read to the end."
                    }
                },
                "required": ["path"]
            }),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path' argument".into()))?
            .to_string();
        let max_bytes = args["max_bytes"]
            .as_u64()
            .map_or(ctx.config.max_read_bytes, |n| {
                usize::try_from(n).unwrap_or(usize::MAX)
            });
        let max_bytes = max_bytes.min(ctx.config.max_read_bytes);
        let start_line = args["start_line"]
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(1));
        let end_line = args["end_line"]
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        let sandbox = ctx.sandbox.clone();
        super::spawn_blocking_tool(move || {
            fs_read(&sandbox, &path, max_bytes, start_line, end_line)
        })
        .await
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
    fn test_fs_read() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "Hello, Aura!";
        fs::write(dir.path().join("test.txt"), content).unwrap();

        let result = fs_read(&sandbox, "test.txt", 1024, None, None).unwrap();
        assert!(result.ok);
        assert_eq!(&result.stdout[..], content.as_bytes());
    }

    #[test]
    fn test_fs_read_size_limit() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "Hello, Aura!";
        fs::write(dir.path().join("test.txt"), content).unwrap();

        let max_bytes = 5usize;
        let result = fs_read(&sandbox, "test.txt", max_bytes, None, None).unwrap();
        assert!(result.ok, "truncation must surface as a successful result");
        let expected_marker =
            truncation_marker(content.len() - max_bytes, content.len());
        let expected_len = max_bytes + expected_marker.as_bytes().len();
        assert_eq!(
            result.stdout.len(),
            expected_len,
            "body must equal max_bytes plus the truncation marker"
        );
        assert!(
            result.stdout.starts_with(&content.as_bytes()[..max_bytes]),
            "body must begin with the first max_bytes of the file"
        );
        assert!(
            result.stdout.ends_with(expected_marker.as_bytes()),
            "body must end with the standard truncation marker"
        );
        assert_eq!(
            result.metadata.get("truncated").map(String::as_str),
            Some("true"),
            "truncation must be flagged in metadata"
        );
        assert_eq!(
            result.metadata.get("size").map(String::as_str),
            Some(content.len().to_string().as_str()),
            "size metadata must report the full file size, not the truncated length"
        );
    }

    #[test]
    fn test_fs_read_default_cap_truncates_large_file() {
        let (sandbox, dir) = create_test_sandbox();

        let total = 200 * 1024;
        let content = vec![b'a'; total];
        fs::write(dir.path().join("big.txt"), &content).unwrap();

        let max_bytes = 64 * 1024;
        let result = fs_read(&sandbox, "big.txt", max_bytes, None, None).unwrap();
        assert!(result.ok);
        let expected_marker = truncation_marker(total - max_bytes, total);
        let expected_len = max_bytes + expected_marker.as_bytes().len();
        assert_eq!(
            result.stdout.len(),
            expected_len,
            "body must be exactly max_bytes plus the truncation marker"
        );
        assert!(
            result.stdout.ends_with(expected_marker.as_bytes()),
            "body must end with the standard truncation marker"
        );
        assert_eq!(
            result.metadata.get("truncated").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            result.metadata.get("size").map(String::as_str),
            Some(total.to_string().as_str())
        );
    }

    #[test]
    fn test_fs_read_line_range_output_cap() {
        let (sandbox, dir) = create_test_sandbox();

        let line_count = 100usize;
        let line_body_len = 10usize;
        let line_body: String = "a".repeat(line_body_len);
        let lines: Vec<String> = (0..line_count).map(|_| line_body.clone()).collect();
        let content = lines.join("\n");
        fs::write(dir.path().join("lines.txt"), &content).unwrap();

        let file_size = content.len();
        let line_numbered_len = line_count * (6 + 1 + line_body_len) + (line_count - 1);
        let max_bytes = file_size + 100;
        assert!(
            max_bytes >= file_size,
            "max_bytes must accommodate the raw file so the cap fires on the rendered output, not the file"
        );
        assert!(
            line_numbered_len > max_bytes,
            "line-numbered output must exceed max_bytes for this test to exercise the line-render cap"
        );

        let result = fs_read(
            &sandbox,
            "lines.txt",
            max_bytes,
            Some(1),
            Some(line_count),
        )
        .unwrap();
        assert!(result.ok);

        let body = String::from_utf8(result.stdout.to_vec())
            .expect("line-numbered output is always UTF-8");
        let marker_prefix = "\n... [truncated";
        assert!(
            body.contains(marker_prefix),
            "rendered output must carry the truncation marker"
        );
        let marker_offset = body.find(marker_prefix).unwrap();
        assert!(
            marker_offset <= max_bytes,
            "kept body length must not exceed max_bytes (got {marker_offset}, cap {max_bytes})"
        );
        assert_eq!(
            result.metadata.get("truncated").map(String::as_str),
            Some("true"),
            "line-render truncation must surface in metadata"
        );
    }

    #[test]
    fn test_fs_read_binary_content() {
        let (sandbox, dir) = create_test_sandbox();

        let content = vec![0u8, 1, 2, 255, 254, 253];
        fs::write(dir.path().join("binary.bin"), &content).unwrap();

        let result = fs_read(&sandbox, "binary.bin", 1024, None, None).unwrap();
        assert!(result.ok);
        assert_eq!(&result.stdout[..], content.as_slice());
    }

    #[test]
    fn test_fs_read_empty_file() {
        let (sandbox, dir) = create_test_sandbox();

        fs::write(dir.path().join("empty.txt"), "").unwrap();

        let result = fs_read(&sandbox, "empty.txt", 1024, None, None).unwrap();
        assert!(result.ok);
        assert!(result.stdout.is_empty());
    }

    #[test]
    fn test_fs_read_not_a_file() {
        let (sandbox, dir) = create_test_sandbox();

        fs::create_dir(dir.path().join("dir")).unwrap();

        let result = fs_read(&sandbox, "dir", 1024, None, None);
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn test_fs_read_line_range() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "line1\nline2\nline3\nline4\nline5";
        fs::write(dir.path().join("lines.txt"), content).unwrap();

        let result = fs_read(&sandbox, "lines.txt", 1024, Some(2), Some(4)).unwrap();
        assert!(result.ok);
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
        assert!(output.contains("line4"));
        assert!(!output.contains("line1\n"));
        assert!(!output.contains("line5"));
        assert_eq!(result.metadata.get("start_line").unwrap(), "2");
        assert_eq!(result.metadata.get("end_line").unwrap(), "4");
    }

    #[test]
    fn test_fs_read_start_line_only() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "line1\nline2\nline3";
        fs::write(dir.path().join("lines.txt"), content).unwrap();

        let result = fs_read(&sandbox, "lines.txt", 1024, Some(2), None).unwrap();
        assert!(result.ok);
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[test]
    fn test_fs_read_start_line_past_eof() {
        let (sandbox, dir) = create_test_sandbox();

        let content = "line1\nline2";
        fs::write(dir.path().join("lines.txt"), content).unwrap();

        let result = fs_read(&sandbox, "lines.txt", 1024, Some(100), None).unwrap();
        assert!(result.ok);
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("2 lines"));
    }
}
