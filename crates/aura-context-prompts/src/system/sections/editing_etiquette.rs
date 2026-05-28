//! `<editing_etiquette>`-bound section.
//!
//! Codex-derived editing constraints: ASCII default, dirty-worktree
//! stance, git safety, and comment hygiene. Git safety rules
//! previously lived in [`super::dev_loop_workflow`] invariants; this
//! section is the single source of truth.

/// Render the editing-etiquette section wrapped in the canonical envelope.
#[must_use]
pub fn render() -> Option<String> {
    let body = "\
- Default to ASCII when editing or creating files. Only introduce non-ASCII or other Unicode characters when there is a clear justification and the file already uses them.
- You may be in a dirty git worktree. NEVER revert changes you did not make this session unless explicitly requested.
- If you notice unexpected changes you did not make, STOP. In chat mode, ask the user how to proceed. In dev-loop mode, call `task_done` with `no_changes_needed: true` and explain the contradiction in `notes`.
- Never run destructive git commands: `git push --force`, `git reset --hard`, `git clean -fd`, `git checkout --`, or `git config`. Do not touch `.gitignore` to hide build output.
- Comments: rare and purposeful — explain non-obvious intent ahead of complex blocks; never narrate the obvious (no \"// Increment counter\").";
    Some(format!("<editing_etiquette>\n{body}\n</editing_etiquette>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_envelope_with_body() {
        let out = render().expect("editing_etiquette section renders");
        assert!(out.starts_with("<editing_etiquette>\n"));
        assert!(out.ends_with("\n</editing_etiquette>"));
        assert!(!out.contains("<editing_etiquette></editing_etiquette>"));
    }

    #[test]
    fn render_includes_git_safety_rules() {
        let out = render().expect("section renders");
        assert!(out.contains("git push --force"));
        assert!(out.contains("git reset --hard"));
    }

    #[test]
    fn render_includes_comment_hygiene_rule() {
        let out = render().expect("section renders");
        assert!(out.contains("never narrate"));
    }
}
