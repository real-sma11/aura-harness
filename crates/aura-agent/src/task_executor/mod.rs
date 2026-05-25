//! Task-aware tool executor with plan gating, file tracking, self-review,
//! and stub detection.
//!
//! [`TaskToolExecutor`] wraps an inner [`AgentToolExecutor`] to intercept
//! engine-level tools (`task_done`, `submit_plan`, `get_task_context`) and
//! enforce the explore-then-implement workflow.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::agent_runner::FollowUpSuggestion;
use crate::build::{classify_build_errors, error_category_guidance};
use crate::events::AgentLoopEvent;
use crate::file_ops::{self, FileOp};
use crate::planning::{TaskPhase, TaskPlan};
use crate::self_review::SelfReviewGuard;
use crate::types::{
    AgentToolExecutor, AutoBuildResult, BuildBaseline, ToolCallInfo, ToolCallResult,
};
use crate::verify::{infer_default_build_command, infer_default_test_command, TestSuiteOutcome};

const MAX_STUB_FIX_ATTEMPTS: u32 = 2;

/// Maximum number of times the `task_done` hard gate will reject a completion
/// because the project test suite is failing.
///
/// The gate runs the full project test command on every `task_done` call, so
/// each attempt costs a full test-suite execution. The cap exists to bound
/// runaway loops on intractable failures: once the agent has had this many
/// chances to make the suite green it is allowed to surface the failure as a
/// hard `task_failed` with the `dod_test_gate_exhausted` flag set.
pub const MAX_TASK_DONE_TEST_RETRIES: u32 = 8;

/// Environment variable that disables the `task_done` test gate.
///
/// This is an explicit operator escape hatch for emergency/manual runs only.
/// The reader accepts only the literal `"1"`; typo-prone truthy values keep
/// the gate active. Normal projects should configure a test command instead.
pub(crate) const DISABLE_TEST_GATE_ENV: &str = "AURA_DOD_DISABLE_TEST_GATE";

/// Environment variable that overrides the project-configured test command
/// for the `task_done` hard gate.
///
/// This remains an operator override, not a fallback. Empty or whitespace-only
/// values are treated as unset so a shell can clear the override without
/// accidentally disabling the gate.
///
/// Resolution order at gate time, highest precedence first:
///   1. `AURA_DOD_TEST_COMMAND` (this env var, captured at executor construction)
///   2. `Project.test_command` (per-project config)
///   3. `infer_default_test_command(project_root)` (manifest auto-detect)
pub(crate) const TEST_COMMAND_OVERRIDE_ENV: &str = "AURA_DOD_TEST_COMMAND";

/// Read the [`DISABLE_TEST_GATE_ENV`] env var at construction time. Returns
/// `true` only when explicitly set to `"1"`. Captured once per executor so
/// concurrent tests that mutate the global env cannot race the gate.
#[must_use]
pub(crate) fn read_disable_test_gate_env() -> bool {
    std::env::var(DISABLE_TEST_GATE_ENV).ok().as_deref() == Some("1")
}

/// Read the [`TEST_COMMAND_OVERRIDE_ENV`] env var at construction time.
/// Returns the override string when present and non-empty, otherwise `None`.
/// Captured once per executor so concurrent tests that mutate the global env
/// cannot race the gate.
#[must_use]
pub(crate) fn read_test_command_override_env() -> Option<String> {
    let raw = std::env::var(TEST_COMMAND_OVERRIDE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Pluggable test runner used by the `task_done` hard gate.
///
/// Real automatons get [`RealTaskTestRunner`], which shells out to
/// [`crate::verify::run_full_test_suite`]. Unit tests inject a deterministic
/// mock so the gate can be exercised without spinning up a child process.
#[async_trait]
pub(crate) trait TaskTestRunner: Send + Sync + std::fmt::Debug {
    async fn run_tests(
        &self,
        project_root: &Path,
        command: &str,
    ) -> anyhow::Result<TestSuiteOutcome>;
}

/// Default [`TaskTestRunner`] implementation that runs the project's test
/// command via the verify module.
#[derive(Debug, Default)]
pub(crate) struct RealTaskTestRunner;

#[async_trait]
impl TaskTestRunner for RealTaskTestRunner {
    async fn run_tests(
        &self,
        project_root: &Path,
        command: &str,
    ) -> anyhow::Result<TestSuiteOutcome> {
        crate::verify::run_full_test_suite(project_root, command).await
    }
}

mod handlers;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// TaskToolExecutor
// ---------------------------------------------------------------------------

/// Tool executor that layers plan gating, file-op tracking, self-review,
/// and stub detection on top of a delegated executor.
pub(crate) struct TaskToolExecutor {
    /// Inner executor that handles filesystem and search tools.
    pub inner: Arc<dyn AgentToolExecutor>,
    /// Path to the project root for build and stub checks.
    pub project_folder: String,
    /// Build command (from project config or auto-detected).
    pub build_command: Option<String>,
    /// Test command used by the `task_done` hard gate. When `None`, the gate
    /// tries to infer one from the project manifest and otherwise no-ops with
    /// a warning. Configured per project via `Project.test_command`.
    pub test_command: Option<String>,
    /// Operator override for the project test command, captured at executor
    /// construction from [`TEST_COMMAND_OVERRIDE_ENV`]. When `Some`, this
    /// wins over [`Self::test_command`] and any inferred default. Captured
    /// once so concurrent tests that mutate the global env don't race the
    /// gate.
    pub test_command_override: Option<String>,
    /// Pre-built task context for `get_task_context` handler.
    pub task_context: String,
    /// Tracked file operations for stub detection.
    pub tracked_file_ops: Arc<Mutex<Vec<FileOp>>>,
    /// Completion notes accumulated by `task_done`.
    pub notes: Arc<Mutex<String>>,
    /// Follow-up suggestions from `task_done`.
    pub follow_ups: Arc<Mutex<Vec<FollowUpSuggestion>>>,
    /// Counter for stub-fix rejection attempts.
    pub stub_fix_attempts: Arc<Mutex<u32>>,
    /// Counter for `task_done` rejections caused by the all-tests-pass gate.
    /// Bounded by [`MAX_TASK_DONE_TEST_RETRIES`].
    pub test_gate_attempts: Arc<Mutex<u32>>,
    /// Pluggable runner the `task_done` hard gate uses to execute the
    /// project's test suite. Tests inject a mock; real automatons use
    /// [`RealTaskTestRunner`].
    pub test_runner: Arc<dyn TaskTestRunner>,
    /// When `true`, the `task_done` test gate is skipped entirely. Captured
    /// at construction time from the [`DISABLE_TEST_GATE_ENV`] environment
    /// variable so that concurrent unit tests cannot race on the global env.
    pub disable_test_gate: bool,
    /// Current task phase (explore vs implement).
    pub task_phase: Arc<Mutex<TaskPhase>>,
    /// Self-review guard tracking writes vs reads.
    pub self_review: Arc<Mutex<SelfReviewGuard>>,
    /// Optional event channel for status messages.
    pub event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
    /// Set to true when the agent explicitly declares no file changes are
    /// required for this task (via `no_changes_needed` in `task_done` input).
    pub no_changes_needed: Arc<Mutex<bool>>,
    /// Set to true when the `task_done` test gate has rejected
    /// [`MAX_TASK_DONE_TEST_RETRIES`] consecutive completions and is now
    /// surfacing the failure as a hard `task_failed`. Consumed by automatons
    /// that emit DoD telemetry.
    pub dod_test_gate_exhausted: Arc<Mutex<bool>>,
    /// Rolling counters for recent tool call outcomes (success / error).
    pub recent_tool_outcomes: Arc<Mutex<RecentToolOutcomes>>,
    /// Shared flag observed by the agent loop's
    /// [`crate::agent_loop::AgentLoopConfig::phase_reset_signal`]: when
    /// the loop sees it set, [`crate::agent_loop::LoopState::begin_iteration`]
    /// zeroes the exploration/read-guard counters, bumps the allowance
    /// with the implement-phase bonus, and arms the post-plan
    /// exploration hard block.
    ///
    /// As of harness-v2 the production `execute_task_tracked` constructor
    /// pre-seeds this to `true` so the very first iteration of every task
    /// gets the fresh-budget reset. `handle_submit_plan` still flips it
    /// back to `true` on an accepted plan so an explicit mid-run plan
    /// also resets the budget. Defaults to `false` when the executor is
    /// built standalone (e.g. unit tests) so legacy fixtures keep their
    /// pre-v2 semantics.
    pub reset_explore_on_phase_change: Arc<AtomicBool>,
}

/// Capacity of the rolling outcome window. Sized to comfortably cover a
/// single implementation burst (submit_plan + ~10-15 file/search ops +
/// a handful of retries) without letting errors from earlier in the
/// turn veto a `task_done` that the agent has clearly recovered from.
pub(crate) const RECENT_OUTCOMES_WINDOW: usize = 16;

/// One slot in the [`RecentToolOutcomes`] ring buffer.
#[derive(Debug, Clone, Copy)]
struct OutcomeEntry {
    is_error: bool,
    /// True when the error was a policy denial (e.g. `run_command` not
    /// in the allow-list) rather than a tool that actually executed
    /// and returned a non-zero exit. Policy denials are not counted as
    /// "real" failures against the pervasive-error guard because the
    /// agent has nothing to fix — it just needs to stop calling the
    /// blocked tool.
    policy_denied: bool,
}

/// Rolling window of recent tool-call outcomes used by the pervasive-
/// error guard on `task_done`.
///
/// Earlier revisions kept monotonic `total` / `errors` counters for the
/// lifetime of the `TaskToolExecutor`, which meant a noisy exploration
/// phase (e.g. a handful of `read_file` calls against directories,
/// plus policy-denied `run_command` attempts) could push the error
/// ratio over the 70% threshold and reject a `task_done` that
/// otherwise represented successful work.
///
/// The ring buffer keeps only the last [`RECENT_OUTCOMES_WINDOW`]
/// outcomes, and `reset()` is called when the executor transitions to
/// the implementing phase (after `submit_plan`) so prior exploration
/// noise never influences the completion check.
#[derive(Debug, Default)]
pub(crate) struct RecentToolOutcomes {
    entries: std::collections::VecDeque<OutcomeEntry>,
    /// True when the most recent `run_command` actually executed and
    /// returned a non-zero exit. Policy-denied commands *do not* set
    /// this flag because nothing ran.
    pub(crate) last_command_failed: bool,
}

impl RecentToolOutcomes {
    /// Record one tool-call outcome.
    pub fn record(&mut self, tool_name: &str, is_error: bool, content: &str) {
        let policy_denied = is_error && is_policy_denial(content);
        while self.entries.len() >= RECENT_OUTCOMES_WINDOW {
            self.entries.pop_front();
        }
        self.entries.push_back(OutcomeEntry {
            is_error,
            policy_denied,
        });
        if tool_name == "run_command" {
            // Policy denial means the command never ran — the agent
            // isn't staring at a broken build, it just hit an
            // allow-list. Treat it as a non-event for the "last
            // command failed" signal the completion guard reads.
            self.last_command_failed = is_error && !policy_denied;
        }
    }

    /// Total outcomes currently in the window.
    #[must_use]
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// Errors in the window that were *not* policy denials.
    #[must_use]
    pub fn real_errors(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_error && !e.policy_denied)
            .count()
    }

    /// Clear the window. Called on plan acceptance so the noisy
    /// exploration phase never votes against a clean implementation
    /// phase.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.last_command_failed = false;
    }
}

/// Heuristic: does this tool-result content look like a policy denial
/// rather than an actual tool failure?
///
/// `aura-kernel`'s `PolicyVerdict::Deny` stamps one of a small set of
/// reasons into the tool result. We match on those prefixes so the
/// completion guard can distinguish "you tried to run a blocked tool"
/// from "the command you ran exited non-zero". The matching is
/// deliberately conservative — if a downstream tool happens to emit
/// one of these strings in its stderr, treating it as a policy denial
/// is safe (it just means we don't count it against the error ratio),
/// and the `last_command_failed` short-circuit is still protected by
/// the tool-name check on `run_command`.
fn is_policy_denial(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("Tool '") && trimmed.contains("is not allowed")
        || trimmed.starts_with("Tool '") && trimmed.contains("requires approval")
        || trimmed.starts_with("Policy denied")
        || trimmed.starts_with("permissions: requires capability")
        || trimmed.starts_with("permissions: target out of scope")
}

#[async_trait]
impl AgentToolExecutor for TaskToolExecutor {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        let mut delegated_indices: Vec<usize> = Vec::new();

        for (i, tc) in tool_calls.iter().enumerate() {
            match tc.name.as_str() {
                "task_done" | "get_task_context" | "submit_plan" | "apply_patch" => {}
                "write_file" | "edit_file" | "delete_file" => {
                    self.track_file_op(&tc.name, &tc.input).await;
                    if let Some(path) = tc.input.get("path").and_then(|v| v.as_str()) {
                        self.self_review.lock().await.record_write(path);
                    }
                    delegated_indices.push(i);
                }
                _ => {
                    self.track_file_op(&tc.name, &tc.input).await;
                    if tc.name == "read_file" {
                        if let Some(path) = tc.input.get("path").and_then(|v| v.as_str()) {
                            self.self_review.lock().await.record_read(path);
                        }
                    }
                    delegated_indices.push(i);
                }
            }
        }

        // Delegate non-special tools to inner executor
        let delegated_calls: Vec<ToolCallInfo> = delegated_indices
            .iter()
            .map(|&i| tool_calls[i].clone())
            .collect();
        let delegated_results = if delegated_calls.is_empty() {
            Vec::new()
        } else {
            self.inner.execute(&delegated_calls).await
        };

        let mut delegated_iter = delegated_results.into_iter();
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut stop = false;

        for tc in tool_calls.iter() {
            match tc.name.as_str() {
                "task_done" => {
                    self.handle_task_done(tc, &mut results, &mut stop).await;
                }
                "get_task_context" => {
                    self.handle_get_context(tc, &mut results);
                }
                "submit_plan" => {
                    self.handle_submit_plan(tc, &mut results).await;
                }
                "apply_patch" => {
                    self.handle_apply_patch(tc, &mut results).await;
                    // Pipe the result through the same status emit +
                    // rolling-outcome accounting as delegated tools so
                    // the live UI sees `[tool: apply_patch -> ok]` and
                    // the pervasive-error guard tracks success/failure.
                    if let Some(result) = results.last() {
                        self.emit_tool_status(tc, result);
                        let mut outcomes = self.recent_tool_outcomes.lock().await;
                        outcomes.record(&tc.name, result.is_error, &result.content);
                    }
                }
                _ => {
                    if let Some(result) = delegated_iter.next() {
                        self.emit_tool_status(tc, &result);
                        {
                            let mut outcomes = self.recent_tool_outcomes.lock().await;
                            outcomes.record(&tc.name, result.is_error, &result.content);
                        }
                        results.push(result);
                    }
                }
            }
        }

        if stop {
            for r in &mut results {
                r.stop_loop = true;
            }
        }

        results
    }

    async fn auto_build_check(&self) -> Option<AutoBuildResult> {
        let project_root = Path::new(&self.project_folder);
        let cmd = self
            .build_command
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .or_else(|| infer_default_build_command(project_root))?;

        self.emit_text(format!("\n[auto-build: {cmd}]\n"));

        match crate::verify::run_build_command(project_root, &cmd, None).await {
            Ok(result) => {
                let mut output = String::new();
                if !result.stdout.is_empty() {
                    output.push_str(&result.stdout);
                }
                if !result.stderr.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&result.stderr);
                }
                let output = if result.success {
                    output
                } else {
                    let pf = self.project_folder.clone();
                    tokio::task::spawn_blocking(move || {
                        handlers::enrich_compiler_output_sync(&pf, &output)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!("spawn_blocking panicked in enrich_compiler_output: {e}");
                        String::new()
                    })
                };
                Some(AutoBuildResult {
                    success: result.success,
                    output,
                    error_count: 0,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "auto-build check failed to execute");
                None
            }
        }
    }

    async fn capture_build_baseline(&self) -> Option<BuildBaseline> {
        let project_root = Path::new(&self.project_folder);
        let cmd = self
            .build_command
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .or_else(|| infer_default_build_command(project_root))?;

        match crate::verify::run_build_command(project_root, &cmd, None).await {
            Ok(result) if !result.success => {
                let sigs = BuildBaseline::extract_signatures(&result.stderr);
                tracing::info!(
                    count = sigs.len(),
                    "captured build baseline with pre-existing errors",
                );
                Some(BuildBaseline {
                    error_signatures: sigs,
                })
            }
            Ok(_) => Some(BuildBaseline::default()),
            Err(e) => {
                tracing::warn!(error = %e, "failed to capture build baseline");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a concise hint for a tool call's arguments (for status logging).
pub(crate) fn format_tool_arg_hint(tc: &ToolCallInfo) -> String {
    match tc.name.as_str() {
        "read_file" => {
            let path = tc.input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let start = tc
                .input
                .get("start_line")
                .and_then(serde_json::Value::as_u64);
            let end = tc.input.get("end_line").and_then(serde_json::Value::as_u64);
            match (start, end) {
                (Some(s), Some(e)) => format!("{path}:{s}-{e}"),
                (Some(s), None) => format!("{path}:{s}-end"),
                (None, Some(e)) => format!("{path}:1-{e}"),
                (None, None) => path.to_string(),
            }
        }
        "write_file" | "edit_file" | "delete_file" => tc
            .input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "apply_patch" => {
            // The full patch envelope is too large for a status line;
            // just surface a byte count so the UI shows "apply_patch(1.2kB)".
            let bytes = tc
                .input
                .get("patch")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            format!("{}B", bytes)
        }
        "list_files" => tc
            .input
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "search_code" => {
            let pattern = tc
                .input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ctx = tc
                .input
                .get("context_lines")
                .and_then(serde_json::Value::as_u64);
            if let Some(c) = ctx {
                format!("{pattern}, context={c}")
            } else {
                pattern.to_string()
            }
        }
        "run_command" => tc
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Check if build output looks like compiler errors (Rust or TypeScript).
pub(crate) fn looks_like_compiler_errors(output: &str) -> bool {
    let has_rust_errors = output.contains("error[E") && output.contains("-->");
    let has_generic_errors = output.contains("error:") && output.contains("-->");
    let has_ts_errors = output.contains("TS2") && output.contains("error TS");
    has_rust_errors || has_generic_errors || has_ts_errors
}
