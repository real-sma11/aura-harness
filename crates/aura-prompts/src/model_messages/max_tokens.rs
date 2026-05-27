//! Synthetic `tool_result` bodies injected when the model response
//! is truncated mid-tool by `max_tokens`. The agent loop recovers
//! the partial tool_use, then renders one of these bodies so the
//! model sees a concrete next-step recovery hint.

/// Render the synthetic body for a `write_file` truncation.
#[must_use]
pub fn write_file_truncation_with_path(path: &str) -> String {
    format!(
        "Error: Response was truncated (max_tokens) mid-`write_file`. \
         Target path: `{path}`. Partial content (if any) is NOT on disk. \
         Next turn: call `edit_file` on `{path}` with `append_after_eof` to add \
         remaining content incrementally, or call `write_file` with only the \
         skeleton (module-doc + imports + one stub) and switch to `edit_file` \
         appends for the rest."
    )
}

/// Body for a `write_file` truncation when the partial input did not
/// surface a recoverable target path.
pub const WRITE_FILE_TRUNCATION_NO_PATH: &str =
    "Error: Response was truncated (max_tokens) mid-`write_file` \
     (no target path recovered). Next turn: retry with the skeleton \
     (module-doc + imports + one stub) and use `edit_file` \
     `append_after_eof` for the rest.";

/// Render the synthetic body for an `edit_file` truncation.
#[must_use]
pub fn edit_file_truncation_with_path(path: &str) -> String {
    format!(
        "Error: Response was truncated (max_tokens) mid-`edit_file`. \
         Target path: `{path}`. No changes were applied on disk. \
         Next turn: split the edit into TWO smaller `edit_file` calls \
         (e.g. change one function or block at a time) rather than one \
         large diff. Your next `max_tokens` budget is restored to full \
         for the retry, but each individual tool call should fit in a \
         few hundred lines of diff."
    )
}

/// Body for an `edit_file` truncation when the partial input did not
/// surface a recoverable target path.
pub const EDIT_FILE_TRUNCATION_NO_PATH: &str =
    "Error: Response was truncated (max_tokens) mid-`edit_file` \
     (no target path recovered). Next turn: retry with a smaller, \
     targeted edit scoped to a single function or block.";

/// Render the synthetic body for a generic / unknown tool truncation.
#[must_use]
pub fn generic_tool_truncation(tool_name: &str) -> String {
    format!(
        "Error: Response was truncated (max_tokens). Tool '{tool_name}' was not executed. \
         Please try again with a simpler approach or break the task into smaller steps."
    )
}
