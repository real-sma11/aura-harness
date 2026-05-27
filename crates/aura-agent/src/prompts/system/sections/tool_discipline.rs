//! `<tool_discipline>`-bound section.
//!
//! The historical tool-call discipline prose was deleted by the 2026-05
//! cook-loop strip alongside several of the runtime safety valves it
//! described (`ForceToolCallNextTurn`, the narration budget, the
//! read-only-streak `STOP READING` nudge, the `cargo` subcommand
//! denylist). PR C kept the section module wired into
//! [`super::super::SystemPromptBuilder`] so a future refill would not
//! have to touch the call graph.
//!
//! This follow-up refills [`render`] with the *narrow* subset of
//! tool-call patterns the harness still enforces at runtime today,
//! plus one model-shaping anti-re-read rule that is tightly tied to
//! the write tools' failure-reporting contract:
//!
//! - The anti-re-read rule: a successful `write_file` / `edit_file`
//!   does not need a confirming `read_file`. The tools report failure
//!   via `is_error = true` on the tool_result; trust that signal.
//!   Model-shaping only — the harness does not reject a redundant
//!   read — but it lives here because it describes the tool surface's
//!   own success / failure contract.
//! - The 32_000-byte `write_file` chunk guard in
//!   [`crate::agent_loop::tool_pipeline::partition_oversized_writes`]
//!   (constant: [`aura_config::WRITE_FILE_CHUNK_BYTES`]). Oversized
//!   calls are short-circuited with `is_error = true` and never touch
//!   disk.
//! - The compaction-redaction guards in `aura-tools`
//!   ([`fs_write`](../../../../../aura-tools/src/fs_tools/write.rs) and
//!   the `edit_file` executor) that reject any call which echoes the
//!   `<<<AURA_ELIDED_…>>>` placeholder or carries the `_redacted`
//!   field marker injected by `aura-compaction` for oversized prior
//!   inputs.
//!
//! Rules that the original prose enumerated but the harness no longer
//! enforces (narration budget, force-tool-next-turn, alternation-overlap
//! `search_code` rejection, `cargo` subcommand denial via `run_command`)
//! are deliberately omitted — re-advertising them would mislead the
//! model about what the runtime actually rejects.
//!
//! Layer A also dropped the historical "for larger files prefer the
//! unified-patch tool" advice from this section: the dev-loop bundle
//! now ships only the conventional `write_file` / `edit_file` /
//! `delete_file` writers, and the `write_file` + `edit_file`
//! seed-and-append pattern handles oversized files.

/// Render the tool-discipline section wrapped in the canonical
/// `<tool_discipline>...</tool_discipline>` envelope.
///
/// The body is intentionally short — every bullet corresponds to a
/// concrete runtime gate the harness still enforces today, so the
/// model can map a rejection it sees in a tool result back to the
/// rule that produced it.
#[must_use]
pub(crate) fn render() -> Option<String> {
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
        // Priority A: the model-shaping companion to the Recovery
        // continuation body. The runtime gate fires regardless, but
        // the prompt-side bullet shortens the model's discovery
        // window so the doom loop doesn't even start.
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
        // Re-adding the cargo denylist or narration / force-tool gates
        // would mislead the model: the harness no longer enforces any
        // of these (see the module docstring for the audit trail).
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
