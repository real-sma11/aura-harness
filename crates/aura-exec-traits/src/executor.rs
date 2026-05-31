//! Executor trait, context, error, and decode primitives.
//!
//! Moved from `aura-core` during the Phase 2 architecture refactor.
//! These types live in `aura-kernel` because they introduce side effects
//! (tracing, async) that don't belong in the pure-types crate.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

use aura_core::{Action, ActionId, AgentId, Effect, EffectStatus, ToolResult};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("executor not found: {0}")]
    NotFound(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Executor trait for handling actions.
///
/// Executors are responsible for converting authorized Actions into Effects.
/// They may perform side effects (tools, network calls, etc.) and must
/// return appropriate Effect statuses.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute an action and produce an effect.
    ///
    /// # Errors
    /// Returns error if execution fails. The caller should convert this
    /// to a Failed effect and record it.
    async fn execute(&self, ctx: &ExecuteContext, action: &Action)
        -> Result<Effect, ExecutorError>;

    /// Check if this executor can handle the given action.
    fn can_handle(&self, action: &Action) -> bool;

    /// Get the executor name for logging/debugging.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Context provided to executors when executing an action.
#[derive(Debug, Clone)]
pub struct ExecuteContext {
    /// The agent executing the action
    pub agent_id: AgentId,
    /// The action being executed
    pub action_id: ActionId,
    /// Workspace root for this agent (sandbox root for tools)
    pub workspace_root: PathBuf,
    /// Configuration limits
    pub limits: ExecuteLimits,
}

/// Execution limits enforced by the executor.
#[derive(Debug, Clone)]
pub struct ExecuteLimits {
    /// Maximum bytes to read from files
    pub read_bytes: usize,
    /// Maximum bytes to write to files
    pub write_bytes: usize,
    /// Maximum command execution time
    pub command_timeout: Duration,
    /// Maximum stdout bytes from commands
    pub stdout_bytes: usize,
    /// Maximum stderr bytes from commands
    pub stderr_bytes: usize,
}

impl Default for ExecuteLimits {
    fn default() -> Self {
        Self {
            read_bytes: 5 * 1024 * 1024, // 5MB
            write_bytes: 1024 * 1024,    // 1MB
            command_timeout: Duration::from_secs(10),
            stdout_bytes: 256 * 1024, // 256KB
            stderr_bytes: 256 * 1024, // 256KB
        }
    }
}

impl ExecuteContext {
    /// Create a new execution context.
    #[must_use]
    pub fn new(agent_id: AgentId, action_id: ActionId, workspace_root: PathBuf) -> Self {
        Self {
            agent_id,
            action_id,
            workspace_root,
            limits: ExecuteLimits::default(),
        }
    }

    /// Set custom limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: ExecuteLimits) -> Self {
        self.limits = limits;
        self
    }
}

// ---------------------------------------------------------------------------
// Decode helper
// ---------------------------------------------------------------------------

/// Result of decoding a tool execution effect into displayable text.
#[derive(Debug, Clone)]
pub struct DecodedToolResult {
    /// Text content (stdout on success, stderr on failure).
    pub content: String,
    /// Whether the effect represents an error.
    pub is_error: bool,
    /// Machine-readable result classification.
    pub kind: aura_core::ToolResultKind,
    /// Additional metadata from the tool result, if available.
    pub metadata: HashMap<String, String>,
    /// Optional line diff for file-mutating tools (`fs_write` /
    /// `fs_edit` / `fs_delete`). `None` when the tool didn't compute
    /// counts (every other tool, plus tool failures); consumers must
    /// not interpret `None` as "zero lines changed".
    pub line_diff: Option<aura_core::LineDiff>,
}

/// Resolve a command exit code from either the typed `exit_code` field
/// (set on command failures) or the `exit_code` metadata entry (set on
/// command successes). Returns `None` for non-command tools.
fn command_exit_code(tool_result: &ToolResult) -> Option<i32> {
    tool_result.exit_code.or_else(|| {
        tool_result
            .metadata
            .get("exit_code")
            .and_then(|code| code.parse::<i32>().ok())
    })
}

/// Build the user-visible content string for a committed tool effect.
///
/// Command-style tools (`run_command`, or anything carrying an exit code)
/// are encoded as the structured envelope the dashboard understands —
/// `{ ok, exit_code, stdout, stderr }` — so the UI can render stdout,
/// stderr, and an exit-code badge separately instead of collapsing a real
/// run into a lossy one-line summary like `"Success (no output)"`. The
/// stdout/stderr payloads are kept as plain UTF-8 (not base64) so the
/// autonomous agent loop can still read compiler/test output from its own
/// tool-result context.
///
/// Every other tool keeps its readable text (stdout, then stderr) so file
/// and spec tool cards are unchanged.
fn decode_committed_content(tool_result: &ToolResult) -> String {
    let exit_code = command_exit_code(tool_result);
    let is_command = tool_result.tool == "run_command" || exit_code.is_some();

    if is_command {
        let stdout = String::from_utf8_lossy(&tool_result.stdout);
        let stderr = String::from_utf8_lossy(&tool_result.stderr);
        return serde_json::json!({
            "ok": tool_result.ok,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        })
        .to_string();
    }

    if !tool_result.ok && !tool_result.stderr.is_empty() {
        return String::from_utf8_lossy(&tool_result.stderr).to_string();
    }
    if !tool_result.stdout.is_empty() {
        return String::from_utf8_lossy(&tool_result.stdout).to_string();
    }
    if tool_result.ok {
        "Success (no output)".to_string()
    } else {
        "Tool execution failed (no details)".to_string()
    }
}

/// Decode a tool execution [`Effect`] into text content, error status, and metadata.
///
/// Used by the agent loop's `KernelToolGateway` to convert kernel effects
/// back into user-visible tool results.
#[must_use]
pub fn decode_tool_effect(effect: &Effect) -> DecodedToolResult {
    if effect.status == EffectStatus::Committed {
        match serde_json::from_slice::<ToolResult>(&effect.payload) {
            Ok(tool_result) => {
                let content = decode_committed_content(&tool_result);
                DecodedToolResult {
                    content,
                    is_error: !tool_result.ok,
                    kind: tool_result.kind,
                    metadata: tool_result.metadata,
                    line_diff: tool_result.line_diff,
                }
            }
            Err(e) => {
                let raw = String::from_utf8_lossy(&effect.payload);
                warn!(
                    error = %e,
                    payload_len = effect.payload.len(),
                    "Failed to parse Committed tool effect payload as ToolResult"
                );
                DecodedToolResult {
                    content: format!("Tool result could not be parsed: {e}. Raw: {raw}"),
                    is_error: true,
                    kind: aura_core::ToolResultKind::AgentError,
                    metadata: HashMap::new(),
                    line_diff: None,
                }
            }
        }
    } else {
        let (content, kind) =
            if let Ok(tool_result) = serde_json::from_slice::<ToolResult>(&effect.payload) {
                (
                    String::from_utf8_lossy(&tool_result.stderr).to_string(),
                    tool_result.kind,
                )
            } else {
                let raw = String::from_utf8_lossy(&effect.payload);
                let content = if raw.is_empty() {
                    "Tool execution failed".to_string()
                } else {
                    raw.to_string()
                };
                (content, aura_core::ToolResultKind::AgentError)
            };
        DecodedToolResult {
            content,
            is_error: true,
            kind,
            metadata: HashMap::new(),
            line_diff: None,
        }
    }
}
