//! `<tool_discipline>`-bound section.
//!
//! Body covers the narrow set of tool-call patterns the harness still
//! enforces at runtime today plus one model-shaping anti-re-read rule
//! tightly tied to the write tools' failure-reporting contract.
//! See `crates/aura-prompts/src/system/sections/` history for the
//! rationale on which historical rules were intentionally left out
//! after the 2026-05 cook-loop strip.

/// Render the tool-discipline section wrapped in the canonical
/// `<tool_discipline>...</tool_discipline>` envelope.
#[must_use]
pub fn render() -> Option<String> {
    let body = "\
- Do not re-read a file after a successful `write_file` / `edit_file` / `delete_file`. The tools report failure via `is_error = true` on the tool_result; trust that signal and keep moving.
- `write_file` rejects content over 32000 bytes per call - the harness short-circuits the call and the change never lands on disk. For larger files, seed with `write_file` (<=32000 bytes: module doc + imports + one stub) and append the rest with `edit_file`.
- Prior `write_file` / `edit_file` tool_use blocks in the transcript may have their bulky string fields (`content` / `old_text` / `new_text`) stripped to a `_redacted` marker or `<<<AURA_ELIDED_...>>>` placeholder so the transcript fits in context. Always re-emit the real bytes - calls that copy the placeholder verbatim are rejected before anything touches disk.
- If `edit_file` returns `is_error=true` with \"needle not found\" or `write_file` returns \"path not found\": re-read the target file ONCE to get the real bytes, then retry with exact text. Do not keep guessing - the file shape may have changed since you last read it.";
    Some(format!("<tool_discipline>\n{body}\n</tool_discipline>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_envelope_with_body() {
        let out = render().expect("tool_discipline section now renders content");
        assert!(
            out.starts_with("<tool_discipline>\n"),
            "section must open with the canonical envelope: {out}"
        );
        assert!(
            out.ends_with("\n</tool_discipline>"),
            "section must close with the canonical envelope: {out}"
        );
        assert!(
            !out.contains("<tool_discipline></tool_discipline>"),
            "envelope must never collapse to an empty tag: {out}"
        );
    }

    #[test]
    fn render_includes_write_file_chunk_cap_rule() {
        let out = render().expect("section renders");
        assert!(
            out.contains("32000 bytes"),
            "chunk-cap bullet must surface the byte limit verbatim: {out}"
        );
        assert!(
            out.contains("`write_file`"),
            "chunk-cap bullet must name the gated tool: {out}"
        );
    }

    #[test]
    fn render_includes_redaction_placeholder_rule() {
        let out = render().expect("section renders");
        assert!(
            out.contains("_redacted"),
            "redaction bullet must reference the `_redacted` marker: {out}"
        );
        assert!(
            out.contains("AURA_ELIDED"),
            "redaction bullet must reference the elided-content placeholder: {out}"
        );
        assert!(
            out.contains("re-emit"),
            "redaction bullet must steer the model toward re-emitting the real bytes: {out}"
        );
    }

    #[test]
    fn render_includes_recovery_from_rejection_rule() {
        let out = render().expect("section renders");
        assert!(
            out.contains("needle not found"),
            "recovery-from-rejection bullet must name the edit_file failure mode: {out}"
        );
        assert!(
            out.contains("path not found"),
            "recovery-from-rejection bullet must name the write_file failure mode: {out}"
        );
        assert!(
            out.contains("re-read the target file ONCE"),
            "recovery-from-rejection bullet must steer the model toward a single re-read: {out}"
        );
    }

    #[test]
    fn render_omits_stripped_rules() {
        let out = render().expect("section renders");
        assert!(
            !out.contains("cargo check"),
            "cargo subcommand denial is no longer enforced and must not return to the prompt: {out}"
        );
        assert!(
            !out.to_ascii_lowercase().contains("narration"),
            "narration budget was stripped from the runtime and must not return to the prompt: {out}"
        );
        assert!(
            !out.contains("alternation"),
            "alternation-overlap rejection is not enforced and must not return to the prompt: {out}"
        );
    }
}
