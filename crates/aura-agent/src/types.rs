//! Core types for the agent orchestration layer.

use async_trait::async_trait;
use std::sync::Arc;

/// Normalized file mutation kind for turn-level reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Create,
    Modify,
    Delete,
}

/// A single file mutation observed during execution.
///
/// `lines_added` / `lines_removed` are populated by tools that can compute
/// a diff cheaply (currently `edit_file`, which has both `old_text` and
/// `new_text` in its input). Tools that can't (e.g. `write_file` without
/// pre-content, `delete_file` after the file is gone) leave the counts
/// at 0 — downstream consumers must treat 0 as "unknown", not "no
/// change".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Information about a tool call to be executed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallInfo {
    /// Tool use ID from the model.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool arguments as JSON.
    pub input: serde_json::Value,
}

/// Result of executing a single tool call.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    /// Tool use ID.
    pub tool_use_id: String,
    /// Result content (text or error message).
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Machine-readable result classification.
    pub kind: aura_core::ToolResultKind,
    /// When true, the loop terminates after processing all results in this batch.
    /// Used by engine tools like `task_done` to signal task completion.
    pub stop_loop: bool,
    /// File mutations performed by this tool call, if known.
    pub file_changes: Vec<FileChange>,
}

impl ToolCallResult {
    /// Create a successful result.
    #[must_use]
    pub fn success(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
        }
    }

    /// Create an error result.
    #[must_use]
    pub fn error(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
            kind: aura_core::ToolResultKind::AgentError,
            stop_loop: false,
            file_changes: Vec::new(),
        }
    }

    /// Create a structural compaction/redaction error result.
    #[must_use]
    pub fn compaction_structural(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error: true,
            kind: aura_core::ToolResultKind::CompactionStructural,
            stop_loop: false,
            file_changes: Vec::new(),
        }
    }

    /// Attach file-change metadata to a tool result.
    #[must_use]
    pub fn with_file_changes(mut self, file_changes: Vec<FileChange>) -> Self {
        self.file_changes = file_changes;
        self
    }
}

/// Per-bucket token estimates surfaced on [`AgentLoopResult`] and
/// forwarded to clients via `aura_protocol::ContextBreakdown`. Buckets
/// follow the same `chars / CHARS_PER_TOKEN` heuristic that produces
/// [`AgentLoopResult::estimated_context_tokens`], so they are
/// directly comparable on the wire.
///
/// `mcp_tokens` is reserved for MCP integration (see plan); the harness
/// does not populate it today.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentContextBreakdown {
    pub system_prompt_tokens: u64,
    pub tools_tokens: u64,
    pub skills_tokens: u64,
    pub mcp_tokens: u64,
    pub subagents_tokens: u64,
    pub conversation_tokens: u64,
    /// Cache-read tokens reported by the model provider for the most
    /// recent successful iteration. Surfaced on `AgentLoopResult`'s
    /// per-turn `context_breakdown` so the wire `ContextBreakdown`
    /// can carry hit/miss counts to the UI's context-usage popover.
    pub cache_read_tokens: u64,
    /// Cache-creation tokens reported by the model provider for the
    /// most recent successful iteration.
    pub cache_creation_tokens: u64,
}

/// Result of an automatic build check.
#[derive(Debug, Clone, Default)]
pub struct AutoBuildResult {
    /// Whether the build succeeded.
    pub success: bool,
    /// Build output (stdout + stderr).
    pub output: String,
    /// Number of errors detected.
    pub error_count: usize,
}

/// Captured build error baseline for distinguishing pre-existing from new errors.
#[derive(Debug, Clone, Default)]
pub struct BuildBaseline {
    /// Error signatures from the baseline build.
    pub error_signatures: Vec<String>,
}

impl BuildBaseline {
    /// Annotate build output by diffing against pre-existing errors.
    #[must_use]
    pub fn annotate(&self, output: &str) -> String {
        if self.error_signatures.is_empty() {
            return output.to_string();
        }
        let current_sigs = Self::extract_signatures(output);
        if current_sigs.is_empty() {
            return output.to_string();
        }
        let mut new_count = 0usize;
        let mut preexisting_count = 0usize;
        for sig in &current_sigs {
            if self.error_signatures.contains(sig) {
                preexisting_count += 1;
            } else {
                new_count += 1;
            }
        }
        if preexisting_count == 0 {
            return output.to_string();
        }
        format!(
            "[BASELINE] {new_count} error(s) are NEW (introduced by your changes), \
             {preexisting_count} error(s) are PRE-EXISTING (ignore them). Focus only on the new errors.\n\n{output}",
        )
    }

    /// Extract individual error blocks and produce a normalized signature per block.
    #[must_use]
    pub fn extract_signatures(stderr: &str) -> Vec<String> {
        let mut signatures = Vec::new();
        let mut current_block = String::new();
        for line in stderr.lines() {
            let trimmed = line.trim_start();
            let is_start = trimmed.starts_with("error[E")
                || (trimmed.starts_with("error:") && !trimmed.starts_with("error: aborting"));
            if is_start && !current_block.is_empty() {
                let sig = Self::normalize_block(&current_block);
                if !sig.is_empty() {
                    signatures.push(sig);
                }
                current_block.clear();
            }
            if !current_block.is_empty() || is_start {
                current_block.push_str(line);
                current_block.push('\n');
            }
        }
        if !current_block.is_empty() {
            let sig = Self::normalize_block(&current_block);
            if !sig.is_empty() {
                signatures.push(sig);
            }
        }
        signatures
    }

    fn normalize_block(block: &str) -> String {
        let mut lines: Vec<String> = Vec::new();
        for line in block.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("For more information")
                || trimmed.starts_with("help:")
            {
                continue;
            }
            if trimmed.starts_with("-->") {
                lines.push("-->LOCATION".into());
                continue;
            }
            if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('|') {
                continue;
            }
            if trimmed
                .chars()
                .all(|c| c == '^' || c == '-' || c == ' ' || c == '~' || c == '+')
            {
                continue;
            }
            let normalized = Self::strip_line_col(trimmed);
            if !normalized.is_empty() {
                lines.push(normalized);
            }
        }
        lines.sort();
        lines.dedup();
        lines.join("\n")
    }

    fn strip_line_col(line: &str) -> String {
        let mut result = String::with_capacity(line.len());
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == ':' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                result.push(':');
                result.push('N');
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            } else {
                result.push(chars[i]);
                i += 1;
            }
        }
        result
    }
}

/// Result of the full agent loop execution.
#[derive(Debug, Default)]
pub struct AgentLoopResult {
    /// Whether the loop timed out.
    pub timed_out: bool,
    /// Whether the loop stopped due to insufficient credits.
    pub insufficient_credits: bool,
    /// Whether the loop stopped due to stall detection.
    pub stalled: bool,
    /// LLM error that terminated the loop, if any.
    pub llm_error: Option<String>,
    /// Accumulated assistant text across all iterations.
    pub total_text: String,
    /// Accumulated thinking text across all iterations.
    pub total_thinking: String,
    /// Total input tokens used.
    pub total_input_tokens: u64,
    /// Total output tokens used.
    pub total_output_tokens: u64,
    /// Total cache creation input tokens used across all iterations.
    pub total_cache_creation_input_tokens: u64,
    /// Total cache read input tokens used across all iterations.
    pub total_cache_read_input_tokens: u64,
    /// Best-effort estimate of the current occupied context window in tokens.
    pub estimated_context_tokens: u64,
    /// Per-bucket token estimates that approximately sum to
    /// [`Self::estimated_context_tokens`]. Computed using the same
    /// `chars / CHARS_PER_TOKEN` heuristic; see
    /// [`crate::agent_loop::context`] for the rules. Populated on every
    /// turn that reaches the compaction step (which is every turn that
    /// actually calls the model).
    pub context_breakdown: AgentContextBreakdown,
    /// Net file mutations observed across the turn.
    pub file_changes: Vec<FileChange>,
    /// Number of iterations completed.
    pub iterations: usize,
    /// Final message history.
    pub messages: Vec<aura_reasoner::Message>,
    /// Non-standard stop reason set by the loop when it terminates for a
    /// harness-specific condition (e.g. `"narration_budget_exhausted"`
    /// when Phase 4's narration budget forces a stop). Consumed by the
    /// aura-automaton task validator so it can translate harness-side
    /// stalls into structured `NeedsDecomposition` outcomes without
    /// having to inspect message history.
    pub stop_reason_override: Option<String>,
}

impl AgentLoopResult {
    /// Record a file change, collapsing multiple mutations on the same path
    /// into a single net effect for turn-level reporting.
    ///
    /// Line counts (`lines_added` / `lines_removed`) accumulate across
    /// merged mutations so several edits to the same file in one turn
    /// surface as a single rolled-up diff. The exception is the
    /// Create-then-Delete pairing, where the entry is dropped entirely
    /// (the file existed only transiently within the turn) — line
    /// counts disappear with it, matching the "net effect" semantics.
    pub fn record_file_change(&mut self, change: FileChange) {
        if let Some(existing) = self.file_changes.iter_mut().find(|c| c.path == change.path) {
            existing.lines_added = existing.lines_added.saturating_add(change.lines_added);
            existing.lines_removed = existing.lines_removed.saturating_add(change.lines_removed);
            match (existing.kind, change.kind) {
                (FileChangeKind::Create, FileChangeKind::Modify) => {}
                (FileChangeKind::Create, FileChangeKind::Delete) => {
                    self.file_changes.retain(|c| c.path != change.path);
                }
                (FileChangeKind::Modify, FileChangeKind::Modify) => {}
                (FileChangeKind::Modify, FileChangeKind::Delete) => {
                    existing.kind = FileChangeKind::Delete;
                }
                (FileChangeKind::Delete, FileChangeKind::Create) => {
                    existing.kind = FileChangeKind::Modify;
                }
                (FileChangeKind::Delete, FileChangeKind::Modify) => {
                    existing.kind = FileChangeKind::Modify;
                }
                (_, next) => {
                    existing.kind = next;
                }
            }
            return;
        }

        self.file_changes.push(change);
    }
}

/// Observer notified after every completed agent turn.
///
/// Implementations receive the full `AgentLoopResult` (including message
/// history) so they can perform post-turn work such as memory ingestion.
/// Observers are called **inside** `AgentLoop::run_with_events`, making
/// them impossible to skip regardless of the calling entry point (WS,
/// terminal, worker, etc.).
#[async_trait]
pub trait TurnObserver: Send + Sync {
    async fn on_turn_complete(&self, result: &AgentLoopResult);
}

/// Convenience type for a shared collection of turn observers.
pub type TurnObservers = Vec<Arc<dyn TurnObserver>>;

/// Implementors execute tool calls and optionally provide build integration.
///
/// `aura-harness` provides a default implementation wrapping `ExecutorRouter`.
/// `aura-app` can implement this with project-aware paths, domain tools
/// (spec/task CRUD, dev loop, engine phase gating), and event forwarding.
#[async_trait]
pub trait AgentToolExecutor: Send + Sync {
    /// Execute a batch of tool calls.
    ///
    /// Implementations may:
    /// - Gate certain tools (e.g., writes before `submit_plan`)
    /// - Dispatch domain tools to external services
    /// - Track file operations for stub detection
    /// - Signal loop termination via `stop_loop`
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult>;

    /// Run a lightweight build check (e.g., `cargo check --lib`).
    ///
    /// Returns `None` when build checking is not configured.
    async fn auto_build_check(&self) -> Option<AutoBuildResult> {
        None
    }

    /// Capture current build error state as a baseline for distinguishing
    /// pre-existing errors from newly introduced ones.
    async fn capture_build_baseline(&self) -> Option<BuildBaseline> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentLoopResult, FileChange, FileChangeKind};

    fn fc(path: &str, kind: FileChangeKind) -> FileChange {
        FileChange {
            path: path.into(),
            kind,
            lines_added: 0,
            lines_removed: 0,
        }
    }

    fn fc_lines(path: &str, kind: FileChangeKind, added: u32, removed: u32) -> FileChange {
        FileChange {
            path: path.into(),
            kind,
            lines_added: added,
            lines_removed: removed,
        }
    }

    #[test]
    fn file_change_summary_keeps_net_create() {
        let mut result = AgentLoopResult::default();
        result.record_file_change(fc("src/new.rs", FileChangeKind::Create));
        result.record_file_change(fc("src/new.rs", FileChangeKind::Modify));
        assert_eq!(result.file_changes.len(), 1);
        assert!(matches!(
            result.file_changes[0].kind,
            FileChangeKind::Create
        ));
    }

    #[test]
    fn file_change_summary_drops_create_then_delete() {
        let mut result = AgentLoopResult::default();
        result.record_file_change(fc("src/temp.rs", FileChangeKind::Create));
        result.record_file_change(fc("src/temp.rs", FileChangeKind::Delete));
        assert!(result.file_changes.is_empty());
    }

    #[test]
    fn file_change_summary_turns_delete_then_create_into_modify() {
        let mut result = AgentLoopResult::default();
        result.record_file_change(fc("src/lib.rs", FileChangeKind::Delete));
        result.record_file_change(fc("src/lib.rs", FileChangeKind::Create));
        assert_eq!(result.file_changes.len(), 1);
        assert!(matches!(
            result.file_changes[0].kind,
            FileChangeKind::Modify
        ));
    }

    #[test]
    fn file_change_summary_sums_line_counts_across_merges() {
        let mut result = AgentLoopResult::default();
        result.record_file_change(fc_lines("src/lib.rs", FileChangeKind::Modify, 10, 2));
        result.record_file_change(fc_lines("src/lib.rs", FileChangeKind::Modify, 5, 3));
        assert_eq!(result.file_changes.len(), 1);
        assert_eq!(result.file_changes[0].lines_added, 15);
        assert_eq!(result.file_changes[0].lines_removed, 5);
    }
}
