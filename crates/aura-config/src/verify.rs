//! Verify / build-runner / git read-only knobs.

use std::time::Duration;

use crate::env::{lookup_string, AURA_DOD_TEST_COMMAND};

// ---------------------------------------------------------------------------
// Compile-time constants
// ---------------------------------------------------------------------------

/// Max bytes of build-runner stdout / stderr retained before
/// truncation. Consumed by `aura_agent::verify::runner`.
pub const MAX_OUTPUT_BYTES: usize = 12_000;

/// Default build-runner subprocess timeout in seconds.
pub const BUILD_TIMEOUT_SECS: u64 = 120;

/// Per-fix-prompt codebase-snapshot byte cap. Consumed by
/// `aura_agent::verify::utils::build_error_context_snapshot`.
pub const BUILD_FIX_SNAPSHOT_BUDGET: usize = 30_000;

/// Bytes available for the "Actual API Reference" section emitted by
/// the build-fix error-context resolver.
pub const RESOLVE_BUDGET: usize = 10_240;

/// Maximum source-file matches inspected when resolving a single type
/// referenced in a compiler error.
pub const MAX_TYPE_FILES: usize = 5;

/// Bytes available for the "Error Source Files" section emitted by the
/// build-fix error-context resolver.
pub const ERROR_SOURCE_BUDGET: usize = 15_360;

/// Read-only `git` helper subprocess timeout in seconds (used by
/// `aura_agent::git::list_unpushed_commits` and friends).
pub const GIT_READ_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Verify-layer config: subprocess timeouts, output / snapshot caps,
/// and the `AURA_DOD_TEST_COMMAND` operator override.
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    /// See [`MAX_OUTPUT_BYTES`].
    pub max_output_bytes: usize,
    /// See [`BUILD_TIMEOUT_SECS`].
    pub build_timeout: Duration,
    /// See [`BUILD_FIX_SNAPSHOT_BUDGET`].
    pub build_fix_snapshot_budget: usize,
    /// See [`RESOLVE_BUDGET`].
    pub resolve_budget: usize,
    /// See [`MAX_TYPE_FILES`].
    pub max_type_files: usize,
    /// See [`ERROR_SOURCE_BUDGET`].
    pub error_source_budget: usize,
    /// See [`GIT_READ_TIMEOUT_SECS`].
    pub git_read_timeout: Duration,
    /// env: `AURA_DOD_TEST_COMMAND` (default: `None`)
    ///
    /// Operator override for the project test command used by the
    /// post-`task_done` best-effort test run. When `Some`, this wins
    /// over the per-project config and any inferred default. Empty
    /// or whitespace-only values are treated as unset.
    pub test_command_override: Option<String>,
}

impl VerifyConfig {
    /// Compile-time defaults (no env access).
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            max_output_bytes: MAX_OUTPUT_BYTES,
            build_timeout: Duration::from_secs(BUILD_TIMEOUT_SECS),
            build_fix_snapshot_budget: BUILD_FIX_SNAPSHOT_BUDGET,
            resolve_budget: RESOLVE_BUDGET,
            max_type_files: MAX_TYPE_FILES,
            error_source_budget: ERROR_SOURCE_BUDGET,
            git_read_timeout: Duration::from_secs(GIT_READ_TIMEOUT_SECS),
            test_command_override: None,
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Currently infallible (`AURA_DOD_TEST_COMMAND` is a free-form
    /// string), but returns `Result` for API parity with the other
    /// sub-config constructors.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        cfg.test_command_override = lookup_string(AURA_DOD_TEST_COMMAND);
        Ok(cfg)
    }
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
