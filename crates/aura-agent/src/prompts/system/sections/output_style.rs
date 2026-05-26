//! `<output_style>`-bound section.
//!
//! Codex-derived final-answer formatting rules adapted for aura. Applies to
//! both dev-loop and chat paths so assistant text and `task_done` `notes`
//! stay scannable.

/// Render the output-style section wrapped in the canonical envelope.
#[must_use]
pub(crate) fn render() -> Option<String> {
    let body = "\
Default: be very concise; friendly coding-teammate tone.

- Bullets: use `-`, 4-6 per list, one line each when possible, ordered by importance; keep phrasing consistent and parallel.
- Headers: optional; short Title Case (1-3 words) wrapped in **...**; no blank line before the first bullet; add only if they truly help scanability.
- Monospace: backticks for paths, commands, env vars, and code identifiers; use fenced blocks with an info string for multi-line samples; never combine backticks with **.
- Tone: collaborative, present tense, active voice; self-contained; no \"above/below\" anaphora.
- Don'ts: no nested bullets, no ANSI codes, no emojis.
- File references: workspace-relative or absolute path, optional `:line[:column]`; no `file://`, `vscode://`, or `https://` URIs.
- When emitting `task_done` `notes`, follow these same rules.";
    Some(format!("<output_style>\n{body}\n</output_style>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_envelope_with_body() {
        let out = render().expect("output_style section renders");
        assert!(out.starts_with("<output_style>\n"));
        assert!(out.ends_with("\n</output_style>"));
        assert!(!out.contains("<output_style></output_style>"));
    }

    #[test]
    fn render_includes_task_done_notes_rule() {
        let out = render().expect("section renders");
        assert!(out.contains("task_done"));
        assert!(out.contains("notes"));
    }

    #[test]
    fn render_includes_no_emojis_rule() {
        let out = render().expect("section renders");
        assert!(out.contains("no emojis"));
    }
}
