//! Tool-classification lists and tool-pipeline tunables.
//!
//! Everything in this module either ships as a compile-time `pub const`
//! (the tool lists are pure routing data) or as a field on
//! [`ToolsConfig`] (the heartbeat cadence has an env override).

use std::time::Duration;

use crate::env::{lookup_numeric, AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS};

// ---------------------------------------------------------------------------
// Tool lists
// ---------------------------------------------------------------------------

/// Tools whose successful results can be cached within a single run or
/// turn (read-only). Consumed by
/// `aura_agent::agent_loop::tool_execution::is_cacheable`.
pub const CACHEABLE_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "stat_file",
    "find_files",
    "search_code",
];

/// Tools classified as exploration (read-only, non-modifying).
/// Consumed by `aura_agent::helpers::is_exploration_tool` and the
/// implement-now / repeated-read steering.
pub const EXPLORATION_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "find_files",
    "stat_file",
    "search_code",
];

/// Tools that perform writes (mutations). All three count as forward
/// progress for the read-only steering counters and the
/// `had_any_file_write` latch.
pub const WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "delete_file"];

/// Tools that run commands.
pub const COMMAND_TOOLS: &[&str] = &["run_command"];

// ---------------------------------------------------------------------------
// Tool-result caching helper
// ---------------------------------------------------------------------------

/// Deterministic cache key for the per-run tool-result cache. Canonical
/// JSON serialization of the tool input is concatenated with the tool
/// name and a `\0` separator so different tools with otherwise-identical
/// inputs can never collide.
#[must_use]
pub fn tool_result_cache_key(tool_name: &str, input: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(input).unwrap_or_else(|_| format!("{input:?}"));
    format!("{tool_name}\0{canonical}")
}

// ---------------------------------------------------------------------------
// Tool-pipeline tunables
// ---------------------------------------------------------------------------

/// Write-file content per-turn chunk cap. Calls exceeding this size are
/// short-circuited with a synthetic error that asks the agent to write
/// a skeleton first and use `edit_file` appends for the rest.
pub const WRITE_FILE_CHUNK_BYTES: usize = 32_000;

/// Hard ceiling on `write_file` content size (reserved for future
/// executor-side enforcement; currently equal to
/// [`WRITE_FILE_CHUNK_BYTES`] so callers see one effective limit).
pub const WRITE_FILE_HARD_MAX_BYTES: usize = 32_000;

/// Per-tool error-preview byte cap surfaced to the user-visible event
/// stream. Consumed by `aura_agent::agent_loop::tool_execution`.
pub const TOOL_ERROR_PREVIEW_LIMIT: usize = 1024;

/// After a successful write to a path, how many additional `read_file`
/// calls on that same path may be served before the read-after-write
/// allowance is exhausted.
pub const READS_AFTER_WRITE_ALLOWANCE: u8 = 3;

/// Default tool heartbeat cadence (10s) when
/// `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS` is unset.
pub const DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Minimum heartbeat cadence; below this the pump would degenerate
/// into a hot loop / drown the broadcast in heartbeats.
pub const MIN_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 1;

/// Maximum heartbeat cadence; above this would exceed the server's
/// idle ceiling and defeat the heartbeat's purpose.
pub const MAX_TOOL_HEARTBEAT_INTERVAL_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Tool pipeline configuration: caching surface, write-cap bytes, and
/// the heartbeat cadence.
#[derive(Debug, Clone)]
pub struct ToolsConfig {
    /// See [`CACHEABLE_TOOLS`].
    pub cacheable_tools: &'static [&'static str],
    /// See [`EXPLORATION_TOOLS`].
    pub exploration_tools: &'static [&'static str],
    /// See [`WRITE_TOOLS`].
    pub write_tools: &'static [&'static str],
    /// See [`COMMAND_TOOLS`].
    pub command_tools: &'static [&'static str],
    /// See [`WRITE_FILE_CHUNK_BYTES`].
    pub write_file_chunk_bytes: usize,
    /// See [`WRITE_FILE_HARD_MAX_BYTES`].
    pub write_file_hard_max_bytes: usize,
    /// See [`TOOL_ERROR_PREVIEW_LIMIT`].
    pub tool_error_preview_limit: usize,
    /// See [`READS_AFTER_WRITE_ALLOWANCE`].
    pub reads_after_write_allowance: u8,
    /// env: `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS` (default: 10s,
    /// clamped to [`MIN_TOOL_HEARTBEAT_INTERVAL_SECS`] ..
    /// [`MAX_TOOL_HEARTBEAT_INTERVAL_SECS`])
    pub heartbeat_interval: Duration,
}

impl ToolsConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            cacheable_tools: CACHEABLE_TOOLS,
            exploration_tools: EXPLORATION_TOOLS,
            write_tools: WRITE_TOOLS,
            command_tools: COMMAND_TOOLS,
            write_file_chunk_bytes: WRITE_FILE_CHUNK_BYTES,
            write_file_hard_max_bytes: WRITE_FILE_HARD_MAX_BYTES,
            tool_error_preview_limit: TOOL_ERROR_PREVIEW_LIMIT,
            reads_after_write_allowance: READS_AFTER_WRITE_ALLOWANCE,
            heartbeat_interval: Duration::from_secs(DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS),
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] if `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS`
    /// is non-empty but unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        if let Some(raw) = lookup_numeric::<u64>(AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS)? {
            let clamped = raw.clamp(
                MIN_TOOL_HEARTBEAT_INTERVAL_SECS,
                MAX_TOOL_HEARTBEAT_INTERVAL_SECS,
            );
            cfg.heartbeat_interval = Duration::from_secs(clamped);
        }
        Ok(cfg)
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
