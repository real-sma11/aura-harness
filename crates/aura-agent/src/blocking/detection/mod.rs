//! Blocking detection logic.
//!
//! Each detector examines the current tool call against loop state and
//! returns whether to block it (with a recovery message for the model).

use crate::constants::{
    CMD_FAILURE_BLOCK_THRESHOLD, COMMAND_TOOLS, EXPLORATION_TOOLS, MAX_RANGE_READS_PER_FILE,
    MAX_READS_PER_FILE, WRITE_COOLDOWN_ITERATIONS, WRITE_FAILURE_BLOCK_THRESHOLD, WRITE_TOOLS,
};
use crate::read_guard::ReadGuardState;
use crate::types::ToolCallInfo;
use std::collections::{HashMap, HashSet};

/// Mutable state for blocking detection across iterations.
#[derive(Debug, Default)]
pub struct BlockingContext {
    /// Paths that have been successfully written to in previous iterations.
    pub(crate) written_paths: HashSet<String>,
    /// Per-file write failure counts.
    pub(crate) write_failures: HashMap<String, usize>,
    /// Consecutive command failures across iterations.
    pub(crate) consecutive_cmd_failures: usize,
    /// Per-path write cooldowns (iterations remaining).
    pub(crate) write_cooldowns: HashMap<String, usize>,
    /// Current exploration count.
    pub(crate) exploration_count: usize,
    /// Exploration allowance (may be extended on successful writes).
    pub(crate) exploration_allowance: usize,
    /// Count of write tool calls that had no extractable path (malformed args).
    pub(crate) malformed_write_count: usize,
    /// Most-recently-read file path. Used as a fallback hint when the
    /// model emits a `write_file` / `edit_file` with a missing or empty
    /// `path`: the model almost always wants to operate on a file it
    /// just read, so we surface that path in the block message so the
    /// next attempt has a concrete target rather than repeating the
    /// pathless misfire.
    pub(crate) last_read_path: Option<String>,
    /// Set when `TaskToolExecutor::handle_submit_plan` accepts a plan
    /// and the agent loop observes the resulting reset signal in
    /// `LoopState::begin_iteration`. Gates the exploration hard block
    /// in [`detect_blocked_exploration`]: pre-plan exploration is the
    /// only path the agent has to gather enough context to call
    /// `submit_plan` in the first place, so hard-blocking it wedges
    /// the run (the structural plan gate already rejects writes
    /// pre-plan, so the agent has no legal next tool). After
    /// `submit_plan` succeeds, the gate flips on and now polices
    /// runaway read thrash during the implementation phase, which is
    /// what the gate was designed for. Defaults to `false`; flipped
    /// once and never cleared for the rest of the run.
    pub(crate) plan_submitted: bool,
}

impl BlockingContext {
    /// Create a new blocking context with the given exploration allowance.
    #[must_use]
    pub fn new(exploration_allowance: usize) -> Self {
        Self {
            exploration_allowance,
            ..Self::default()
        }
    }

    /// Decrement all write cooldowns, removing expired ones.
    pub(crate) fn decrement_cooldowns(&mut self) {
        self.write_cooldowns.retain(|_, v| {
            *v = v.saturating_sub(1);
            *v > 0
        });
    }

    /// Record a successful write to extend exploration allowance and reset read guards.
    pub(crate) fn on_write_success(&mut self, path: &str, read_guard: &mut ReadGuardState) {
        self.written_paths.insert(path.to_string());
        self.write_failures.remove(path);
        self.exploration_allowance = self.exploration_allowance.saturating_add(2);
        read_guard.reset_for_path(path);
    }

    /// Record a write failure.
    pub(crate) fn on_write_failure(&mut self, path: &str) {
        let count = self.write_failures.entry(path.to_string()).or_insert(0);
        *count += 1;
        if *count >= WRITE_FAILURE_BLOCK_THRESHOLD {
            self.write_cooldowns
                .insert(path.to_string(), WRITE_COOLDOWN_ITERATIONS);
        }
    }

    /// Record a write tool call with missing/invalid path.
    pub(crate) fn on_malformed_write(&mut self) {
        self.malformed_write_count += 1;
    }

    /// Record the path the model just read so a subsequent pathless
    /// `write_file` / `edit_file` can be nudged toward it.
    pub(crate) fn on_read_path(&mut self, path: &str) {
        if !path.is_empty() {
            self.last_read_path = Some(path.to_string());
        }
    }

    /// Best-effort hint for a pathless write, preferring the most
    /// recently read file (which the model almost always intends to
    /// edit) and falling back to any previously-written path.
    pub(crate) fn pathless_write_hint(&self) -> Option<&str> {
        if let Some(path) = self.last_read_path.as_deref() {
            return Some(path);
        }
        self.written_paths.iter().next().map(String::as_str)
    }

    /// Record a command result (success or failure).
    pub(crate) fn on_command_result(&mut self, success: bool) {
        if success {
            self.consecutive_cmd_failures = 0;
        } else {
            self.consecutive_cmd_failures += 1;
        }
    }

    /// Flip the `plan_submitted` latch. Called by the agent loop from
    /// `LoopState::begin_iteration` after it observes the shared
    /// `Arc<AtomicBool>` reset signal flipped by
    /// `TaskToolExecutor::handle_submit_plan`. Idempotent: subsequent
    /// calls are no-ops so callers do not have to guard against
    /// re-observation of the signal across replays.
    pub(crate) fn mark_plan_submitted(&mut self) {
        self.plan_submitted = true;
    }
}

/// Result of checking whether a tool call should be blocked.
#[derive(Debug)]
pub struct BlockCheckResult {
    /// Whether the tool call is blocked.
    pub(crate) blocked: bool,
    /// Recovery message to inject if blocked.
    pub(crate) recovery_message: Option<String>,
}

impl BlockCheckResult {
    const fn allowed() -> Self {
        Self {
            blocked: false,
            recovery_message: None,
        }
    }

    fn blocked(msg: impl Into<String>) -> Self {
        Self {
            blocked: true,
            recovery_message: Some(msg.into()),
        }
    }
}

/// Check if a tool call should be blocked based on all detectors.
pub fn detect_all_blocked(
    tool: &ToolCallInfo,
    ctx: &BlockingContext,
    read_guard: &ReadGuardState,
) -> BlockCheckResult {
    if let Some(result) = detect_missing_required_args(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_blocked_writes(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_blocked_write_failures(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_blocked_commands(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_blocked_exploration(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_blocked_reads(tool, read_guard) {
        if result.blocked {
            return result;
        }
    }

    if let Some(result) = detect_write_cooldowns(tool, ctx) {
        if result.blocked {
            return result;
        }
    }

    BlockCheckResult::allowed()
}

/// Detector 0: Block tools that are missing required arguments.
///
/// When a model emits a tool call with empty input `{}`, downstream detectors
/// all return `None` (inapplicable) instead of blocking, letting the call
/// through to the executor where it fails and disrupts stall detection.
/// This detector catches that case upfront for all tool families.
///
/// The block message for pathless write tools includes a concrete
/// example using the most recently read path (if any) so the model
/// has a specific target to retry against, rather than re-emitting
/// the same pathless call.
fn detect_missing_required_args(
    tool: &ToolCallInfo,
    ctx: &BlockingContext,
) -> Option<BlockCheckResult> {
    // `apply_patch` is the unified dev-loop write primitive: it doesn't
    // take a `path` argument, it takes a single `patch` string containing
    // a multi-file envelope. Validate that shape instead of falling
    // through to the pathless-write block below (which would reject every
    // apply_patch call outright).
    if tool.name == "apply_patch" {
        let patch_ok = tool
            .input
            .get("patch")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        if !patch_ok {
            return Some(BlockCheckResult::blocked(
                "`apply_patch` requires a non-empty `patch` string argument containing \
                 a `*** Begin Patch ... *** End Patch` envelope. Retry with the full \
                 patch payload (Add/Update/Delete directives inside the envelope)."
                    .to_string(),
            ));
        }
        return Some(BlockCheckResult::allowed());
    }
    if WRITE_TOOLS.contains(&tool.name.as_str()) && extract_path(tool).is_none() {
        let hint = ctx.pathless_write_hint().unwrap_or("crates/foo/src/lib.rs");
        let example = match tool.name.as_str() {
            "write_file" => {
                format!("write_file(path=\"{hint}\", content=\"...module contents...\")")
            }
            "edit_file" => {
                format!("edit_file(path=\"{hint}\", old_text=\"...\", new_text=\"...\")")
            }
            "delete_file" => format!("delete_file(path=\"{hint}\")"),
            other => format!("{other}(path=\"{hint}\", ...)"),
        };
        return Some(BlockCheckResult::blocked(format!(
            "`{name}` requires a non-empty `path` argument. Empty strings and whitespace-only \
             paths are rejected because they cannot land on disk. Retry with a concrete file \
             path, e.g. `{example}`. Do NOT re-issue the same pathless call -- the harness will \
             keep blocking it and the task will be rejected by the Definition-of-Done gate if \
             you never follow up with a real-path write.",
            name = tool.name,
            example = example,
        )));
    }
    if COMMAND_TOOLS.contains(&tool.name.as_str()) {
        let has_command = ["command", "shell_script", "program"].iter().any(|key| {
            tool.input
                .get(*key)
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty())
        });
        if !has_command {
            return Some(BlockCheckResult::blocked(format!(
                "`{}` requires executable input. Provide `program` with optional `args`, \
                 or use `shell_script` / legacy `command` for shell execution.",
                tool.name
            )));
        }
    }
    if EXPLORATION_TOOLS.contains(&tool.name.as_str())
        && tool.name == "read_file"
        && extract_path(tool).is_none()
    {
        return Some(BlockCheckResult::blocked(
            "`read_file` requires a `path` argument. Provide the file path to read.".to_string(),
        ));
    }
    None
}

fn extract_path(tool: &ToolCallInfo) -> Option<String> {
    tool.input
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Detector 1: Block duplicate full-file writes to paths already written in this turn.
///
/// Only blocks `write_file` (full rewrites). `edit_file` and `delete_file`
/// are allowed so the agent can make targeted changes after an initial write.
fn detect_blocked_writes(tool: &ToolCallInfo, ctx: &BlockingContext) -> Option<BlockCheckResult> {
    if tool.name != "write_file" {
        return None;
    }
    let path = extract_path(tool)?;
    if ctx.written_paths.contains(&path) {
        Some(BlockCheckResult::blocked(format!(
            "You already wrote to `{path}` in this turn. Use `edit_file` to make targeted changes \
             instead of rewriting the entire file. Prior write_file/edit_file inputs in your \
             history may be redacted with `_redacted` metadata or legacy \
             `<<<AURA_ELIDED_*>>>` placeholders; re-derive old_text/new_text \
             from the task intent, not from the marker."
        )))
    } else {
        Some(BlockCheckResult::allowed())
    }
}

/// Detector 2: Block writes to files that have failed too many times.
fn detect_blocked_write_failures(
    tool: &ToolCallInfo,
    ctx: &BlockingContext,
) -> Option<BlockCheckResult> {
    if !WRITE_TOOLS.contains(&tool.name.as_str()) {
        return None;
    }
    let path = extract_path(tool)?;
    if let Some(&count) = ctx.write_failures.get(&path) {
        if count >= WRITE_FAILURE_BLOCK_THRESHOLD {
            return Some(BlockCheckResult::blocked(format!(
                "Writes to `{path}` have failed {count} times. Try a different approach \
                 or read the file to understand its current state."
            )));
        }
    }
    Some(BlockCheckResult::allowed())
}

/// Detector 3: Block all commands after too many consecutive failures.
fn detect_blocked_commands(tool: &ToolCallInfo, ctx: &BlockingContext) -> Option<BlockCheckResult> {
    if !COMMAND_TOOLS.contains(&tool.name.as_str()) {
        return None;
    }
    if ctx.consecutive_cmd_failures >= CMD_FAILURE_BLOCK_THRESHOLD {
        Some(BlockCheckResult::blocked(format!(
            "Commands have failed {} consecutive times. Fix the underlying issue before \
             running more commands. Review error messages and make code changes first.",
            ctx.consecutive_cmd_failures
        )))
    } else {
        Some(BlockCheckResult::allowed())
    }
}

/// Detector 4: Block exploration tools when allowance is exceeded.
///
/// Stripped (2026-05): previously phase-gated on `plan_submitted` so
/// the hard block only fired after the agent called `submit_plan`. The
/// rationale was that pre-plan reads were the only way to gather
/// context for a credible plan — but round-1 of the strip removed the
/// plan write gate, so the agent can now write at any time. With no
/// gate to flip `plan_submitted`, the latch was permanently open and
/// the read budget became unenforceable: the failing run we used to
/// validate round 1 made 49 tool calls and ~4 minutes of agentic
/// turns without ever writing a file. Drop the latch entirely so the
/// budget is a real ceiling: at `exploration_count >= allowance`
/// every further `read_file`/`search_code`/`list_files` returns
/// "exploration budget exceeded — start writing now". The model's
/// only legal next moves are `write_file` / `edit_file` /
/// `delete_file` / `task_done`, which is the transition the gate was
/// designed to force.
///
/// `BlockingContext::plan_submitted` and `mark_plan_submitted` are
/// kept around for the moment so `handle_submit_plan` can still flip
/// the signal (and so any external caller that reads `plan_submitted`
/// for telemetry keeps working), but this detector no longer reads
/// it. The soft warnings at `allowance - 8` (mild) and
/// `allowance - 4` (strong) still fire so the model gets a heads-up
/// before the wall.
pub(crate) fn detect_blocked_exploration(
    tool: &ToolCallInfo,
    ctx: &BlockingContext,
) -> Option<BlockCheckResult> {
    if !EXPLORATION_TOOLS.contains(&tool.name.as_str()) {
        return None;
    }
    if ctx.exploration_count >= ctx.exploration_allowance {
        Some(BlockCheckResult::blocked(
            "Exploration budget exceeded. You have spent too many iterations reading files \
             and searching without making changes. Start implementing now with the information \
             you have.",
        ))
    } else {
        Some(BlockCheckResult::allowed())
    }
}

/// Detector 5: Block reads that exceed the per-file read guard limits.
fn detect_blocked_reads(
    tool: &ToolCallInfo,
    read_guard: &ReadGuardState,
) -> Option<BlockCheckResult> {
    let is_read = tool.name == "read_file";
    if !is_read {
        return None;
    }
    let path = extract_path(tool)?;
    let is_range = tool.input.get("start_line").is_some() || tool.input.get("end_line").is_some();

    if is_range {
        if read_guard.range_read_count(&path) >= MAX_RANGE_READS_PER_FILE {
            return Some(BlockCheckResult::blocked(format!(
                "You have read ranges of `{path}` too many times. The content should already \
                 be in your context. Use the information you have."
            )));
        }
    } else if read_guard.full_read_count(&path) >= MAX_READS_PER_FILE {
        return Some(BlockCheckResult::blocked(format!(
            "You have read `{path}` in full too many times. The content is already in your \
             context. Use the information you have or read a specific line range."
        )));
    }

    Some(BlockCheckResult::allowed())
}

/// Detector 6: Block writes to paths with active cooldowns.
fn detect_write_cooldowns(tool: &ToolCallInfo, ctx: &BlockingContext) -> Option<BlockCheckResult> {
    if !WRITE_TOOLS.contains(&tool.name.as_str()) {
        return None;
    }
    let path = extract_path(tool)?;
    if let Some(&remaining) = ctx.write_cooldowns.get(&path) {
        if remaining > 0 {
            return Some(BlockCheckResult::blocked(format!(
                "Writes to `{path}` are on cooldown ({remaining} iterations remaining) \
                 due to repeated failures. Try a different approach."
            )));
        }
    }
    Some(BlockCheckResult::allowed())
}

#[cfg(test)]
mod tests;
