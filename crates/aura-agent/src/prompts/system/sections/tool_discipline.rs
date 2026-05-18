//! Phase 4b prompt discipline rules.
//!
//! Codifies the tool-call patterns the harness actively enforces at
//! runtime (Phase 1's 12000-byte `write_file` chunk guard, Phase 2a's
//! `ForceToolCallNextTurn` hint, Phase 4a's narration budget) so the
//! model has the same rules visible in-context and stops triggering
//! the guards in the first place.
//!
//! The literal body is exported as a constant so the assembled-prompt
//! snapshot tests can assert on a single golden string without
//! introducing a new snapshot dependency.

/// Golden text for the `Tool-call discipline` section. The production
/// prompt builders splice this in verbatim; the snapshot tests assert
/// that each bullet survives into the fully assembled prompt.
pub const TOOL_CALL_DISCIPLINE_SECTION: &str = "\
Tool-call discipline:
- write_file must stay at or under 12000 bytes per call. If the file will be larger, create only the module doc + imports + one stub in your first write_file, then use edit_file with append_after_eof for the rest.
- Every write_file / edit_file / delete_file call MUST include a non-empty, real `path` argument. Empty strings and whitespace-only paths are rejected upfront and do not land on disk — the harness treats them as a misfire and the Definition-of-Done gate will reject the run unless you later write to a real path.
- After any read_file or search_code call, your next turn must either call another tool or submit a tool_result-producing action. Do not emit a multi-paragraph plan between tool calls.
- Never issue two search_code calls whose patterns share an alternation term (e.g. \"foo|bar\" then \"bar|baz\"). Consolidate into one refined query first.
- If your last two turns produced no tool calls, the next turn MUST be a single tool call. Prefer read_file or write_file (skeleton) over more exploration.
- Do not invoke run_command for `cargo check`, `cargo build`, `cargo test`, `cargo fmt`, or `cargo clippy`. The harness runs an auto-build step after writes complete and surfaces the output to you; duplicate run_command calls are policy-denied and do not count as verification. If run_command is blocked for a given command, stop retrying it and rely on the auto-build output instead.
- Prior write_file / edit_file tool_use blocks shown in conversation history are redacted: their bulky `content`, `old_text`, and `new_text` fields may be removed and replaced with `_redacted` metadata so the transcript fits in context. Older history may still contain `<<<AURA_ELIDED_*::N_bytes>>>` placeholders. Never copy redaction markers or placeholders verbatim into a new tool call; always supply the real content/edit you want applied. The resolver rejects them with an InvalidArguments error.
";
