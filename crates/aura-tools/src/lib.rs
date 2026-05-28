//! # aura-tools
//!
//! Layer: exec
//!
//! Tool executor and catalog for filesystem and command operations.
//!
//! This crate provides:
//! - `ToolCatalog` for canonical tool metadata
//! - `ToolResolver` for unified tool dispatch (built-in + domain)
//! - Sandboxed filesystem and command operations
//! - Threshold-based async command execution
//!
//! ## Security
//!
//! All filesystem operations are sandboxed to prevent path traversal attacks.
//! Command execution is disabled by default and requires explicit allowlisting.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::unnecessary_literal_bound,
    clippy::option_if_let_else,
    clippy::doc_markdown
)]

pub mod agents;
pub mod automaton_tools;
pub mod catalog;
pub mod definitions;
pub mod domain_tools;
mod error;
mod executor;
pub(crate) mod fs_tools;
pub mod git_tool;
pub mod http_tool;
pub mod intent_classifier;
pub mod permissions;
pub mod resolver;
mod sandbox;
pub mod schema;
pub(crate) mod tool;

pub use catalog::ToolCatalog;
pub use error::ToolError;
pub use executor::ToolExecutor;
pub use fs_tools::{cmd_run_with_threshold, cmd_spawn, output_to_tool_result, ThresholdResult};
pub use git_tool::{
    GitCommitPushTool, GitCommitTool, GitPushTool, GIT_LOCAL_TOOL_NAMES, GIT_REMOTE_TOOL_NAMES,
    GIT_TOOL_NAMES,
};
pub use http_tool::{HttpAuthSource, HttpMethod, HttpToolDefinition, HttpToolDefinitionBuilder};
pub use intent_classifier::{ClassifierError, IntentClassifier};
pub use permissions::{
    load_agent_tool_context, validate_agent_tool_permissions, validate_user_defaults,
    AgentToolContext,
};
pub use resolver::ToolResolver;
pub use sandbox::Sandbox;
pub use schema::{from_claude_json, to_claude_json, SchemaError};
pub use tool::{AgentControlHook, AgentReadHook, SubagentDispatchHook, Tool, ToolContext};

/// Command execution policy for `run_command`.
///
/// This is an execution guardrail, not a catalog visibility or per-tool
/// permission switch. The kernel decides whether `run_command` is enabled for
/// an agent; this policy constrains how the tool may execute after that.
#[derive(Debug, Clone)]
pub struct CommandPolicy {
    /// Enable process spawning inside `run_command`.
    pub enabled: bool,
    /// Allowed commands (empty = all allowed if commands enabled)
    pub command_allowlist: Vec<String>,
    /// Allowed binary names for `run_command`.
    ///
    /// Unlike [`Self::command_allowlist`], which matches the first whitespace
    /// token of the full shell string, this list is checked **after**
    /// resolving `program` through `which`, so it guards against PATH
    /// shadowing tricks (e.g. a malicious `rg` shim dropped next to
    /// `cargo`).
    ///
    /// Empty vec is only valid while command execution is disabled. Once
    /// [`Self::enabled`] is true, an empty list is treated as a
    /// misconfiguration and `run_command` fails closed. Any non-empty list
    /// causes `run_command` to reject programs whose resolved file name is not
    /// present. (Wave 5 / T3.2.)
    pub binary_allowlist: Vec<String>,
    /// When `false` (default), `run_command` refuses the legacy
    /// "empty args treated as shell script" form. Callers must then
    /// supply `program` + non-empty `args`, avoiding the shell-injection
    /// surface that made `command: "git status; rm -rf"` executable.
    /// (Wave 5 / T3.1.)
    pub allow_shell: bool,
    /// Optional allow-list of verbatim shell scripts permitted via
    /// the `shell_script` field of `run_command`. Only consulted once
    /// [`Self::allow_shell`] == `true` has opened the shell path.
    ///
    /// Follows the same "empty allowlist = all allowed" convention as
    /// [`Self::command_allowlist`]:
    ///
    /// - **Empty (default)**: any shell script is accepted, so the
    ///   gate reduces to `allow_shell` alone. This is the form
    ///   Claude-style automatons depend on, because they emit
    ///   `run_command({ command: "cargo check ..." })` where the
    ///   exact script text cannot be enumerated up front.
    /// - **Non-empty**: strict verbatim match. Operators who want to
    ///   pin a specific set of scripts populate this and every other
    ///   script is rejected with `ToolError::Forbidden`.
    ///
    /// The default remains inert because [`Self::allow_shell`] itself
    /// defaults to `false`; flipping `allow_shell` on is the deliberate
    /// security decision, and this field narrows further from there.
    pub allowed_shell_scripts: Vec<String>,
    /// Operator-controlled ceiling for sessions whose per-agent permission
    /// state is effectively full access.
    ///
    /// This flag alone does not bypass any guardrail. Runtime session wiring
    /// must also prove that the calling agent is effectively full access before
    /// setting [`Self::bypass_allowlists`] on a session-scoped config clone.
    pub allow_unrestricted_full_access: bool,
    /// Session-scoped command allowlist bypass.
    ///
    /// Runtime code derives this from `allow_unrestricted_full_access` plus the
    /// effective per-agent tool permissions. Do not set this on process-global
    /// config; `command.enabled`, `allow_shell`, sandboxing, and timeouts remain
    /// enforced even when this is true.
    pub bypass_allowlists: bool,
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self::restricted()
    }
}

impl CommandPolicy {
    /// Fail-closed command policy for library embedders and tests.
    #[must_use]
    pub fn restricted() -> Self {
        Self {
            enabled: false,
            command_allowlist: vec![],
            binary_allowlist: vec![],
            allow_shell: false,
            allowed_shell_scripts: vec![],
            allow_unrestricted_full_access: false,
            bypass_allowlists: false,
        }
    }

    /// Runtime policy for autonomous dev-loop sessions.
    ///
    /// Environment-driven command switches were removed from `aura-runtime`;
    /// production nodes now opt into command execution by selecting this
    /// explicit policy instead of mutating [`ToolConfig::default`] from env.
    #[must_use]
    pub fn for_autonomous_dev_loop() -> Self {
        Self {
            enabled: true,
            command_allowlist: vec![],
            binary_allowlist: default_autonomous_dev_loop_binaries(),
            allow_shell: true,
            allowed_shell_scripts: vec![],
            allow_unrestricted_full_access: true,
            bypass_allowlists: false,
        }
    }
}

/// Binaries the autonomous dev loop needs for verification and common project
/// bootstraps. This list replaces the removed env-based command allowlist for
/// node/runtime startup; callers that need a narrower execution surface can
/// still build a custom [`ToolConfig`] explicitly.
///
/// Platform-specific entries are appended by
/// [`default_autonomous_dev_loop_binaries`] so Windows does not advertise Unix
/// shims such as `ls` that are not guaranteed to resolve as binaries.
pub const DEFAULT_AUTONOMOUS_DEV_LOOP_BINARIES: &[&str] = &[
    "bash",
    "bun",
    "cargo",
    "cargo-clippy",
    "cargo-fmt",
    "cmd",
    "dir",
    "git",
    "go",
    "node",
    "npm",
    "npx",
    "pip",
    "pnpm",
    "powershell",
    "pwsh",
    "pytest",
    "python",
    "python3",
    "rustc",
    "rustfmt",
    "sh",
    "uv",
    "where",
    "yarn",
];

#[cfg(unix)]
const PLATFORM_AUTONOMOUS_DEV_LOOP_BINARIES: &[&str] = &["ls"];

#[cfg(not(unix))]
const PLATFORM_AUTONOMOUS_DEV_LOOP_BINARIES: &[&str] = &[];

/// Build the effective autonomous dev-loop binary allowlist for this platform.
#[must_use]
pub fn default_autonomous_dev_loop_binaries() -> Vec<String> {
    DEFAULT_AUTONOMOUS_DEV_LOOP_BINARIES
        .iter()
        .chain(PLATFORM_AUTONOMOUS_DEV_LOOP_BINARIES.iter())
        .map(|binary| (*binary).to_string())
        .collect()
}

/// Tool execution configuration.
#[derive(Debug, Clone)]
pub struct ToolConfig {
    /// Command execution guardrails.
    pub command: CommandPolicy,
    /// Maximum read bytes
    pub max_read_bytes: usize,
    /// Sync threshold for command execution (milliseconds).
    /// Commands that complete within this threshold return immediately.
    /// Commands that exceed this threshold are moved to async execution.
    pub sync_threshold_ms: u64,
    /// Maximum timeout for async processes (milliseconds).
    pub max_async_timeout_ms: u64,
    /// Per-attempt timeout for `git push` (milliseconds).
    ///
    /// Push is a network operation against Orbit and on a healthy
    /// remote completes well under five seconds. Anything past a
    /// handful of seconds is overwhelmingly the orbit endpoint
    /// being unreachable rather than a slow-but-progressing push,
    /// so the budget is tuned tight: short timeout, few attempts,
    /// fail fast and let the dev-loop / agent move on instead of
    /// burning minutes per task on a wedged network. The knob is
    /// still its own field (and separate from
    /// `max_async_timeout_ms`, clamped to 120s for every other git
    /// operation inside `git_tool::workspace_timeout`) so operators
    /// who run against a slow self-hosted Orbit can raise it
    /// without inflating every other async command's ceiling.
    pub git_push_timeout_ms: u64,
    /// Number of `git push` attempts, including the first. Values
    /// below 1 are coerced to 1 at the call-site. Each retry waits
    /// a short bounded backoff (see `push_backoff_for_attempt`)
    /// before the next attempt; only timeouts and transient network
    /// errors (`could not read from remote`, `RPC failed`,
    /// `early EOF`, `connection reset`) are retried — auth failures
    /// and non-fast-forward rejections short-circuit immediately.
    /// The default is intentionally small ("a couple of quick
    /// tries") rather than three-plus so a dead remote doesn't burn
    /// the agent's wall-clock budget.
    pub git_push_attempts: u32,
    /// Extra filesystem paths to allow beyond the workspace root.
    /// Granted by skill permissions at runtime.
    pub extra_allowed_paths: Vec<std::path::PathBuf>,
}

impl Default for ToolConfig {
    /// Fail-closed defaults: command execution is off and every shell /
    /// script hook is empty. Filesystem tool visibility is no longer controlled
    /// here; the kernel's tri-state policy owns per-tool enablement. An
    /// operator who wants `run_command` must enable [`Self::command`] and
    /// populate `binary_allowlist` with the specific binaries they trust.
    /// Leaving either at the default value keeps `run_command` inert, even if a
    /// delegate proposal reaches [`CmdRunTool::execute`].
    /// (Phase 5 hardening - closes finding M1.)
    fn default() -> Self {
        Self::restricted()
    }
}

impl ToolConfig {
    /// Fail-closed configuration for tests, libraries, and embedders that
    /// have not intentionally selected a runtime execution profile.
    #[must_use]
    pub fn restricted() -> Self {
        Self {
            command: CommandPolicy::restricted(),
            max_read_bytes: 64 * 1024,
            sync_threshold_ms: 5_000,
            max_async_timeout_ms: 600_000,
            // 10s per attempt × 2 attempts + 1s backoff = ~21s
            // worst-case before we surrender and let the agent move
            // on. Bumped down from the legacy 5min × 3 (2s/5s/15s
            // backoff) = ~15min budget that used to wedge dev-loop
            // runs whenever Orbit was unreachable. Operators who
            // host a slow self-hosted Orbit can raise these
            // explicitly on the tool config.
            git_push_timeout_ms: 10_000,
            git_push_attempts: 2,
            extra_allowed_paths: vec![],
        }
    }

    /// Runtime configuration for autonomous dev-loop sessions.
    ///
    /// Keep [`Self::default`] fail-closed for tests and library embedders. The
    /// node/runtime binaries use this constructor when they are expected to run
    /// dev-loop verification commands without env-based permission switches.
    #[must_use]
    pub fn for_autonomous_dev_loop() -> Self {
        Self {
            command: CommandPolicy::for_autonomous_dev_loop(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod default_tests {
    use super::ToolConfig;

    #[test]
    fn default_config_disables_commands() {
        let cfg = ToolConfig::default();
        assert!(
            !cfg.command.enabled,
            "fresh ToolConfig must start with commands disabled"
        );
        assert!(
            cfg.command.binary_allowlist.is_empty(),
            "fresh ToolConfig must have an empty binary_allowlist"
        );
        assert!(
            cfg.command.command_allowlist.is_empty(),
            "fresh ToolConfig must have an empty command_allowlist"
        );
        assert!(
            !cfg.command.allow_shell,
            "fresh ToolConfig must not allow shell scripts"
        );
        assert!(
            cfg.command.allowed_shell_scripts.is_empty(),
            "fresh ToolConfig must have an empty allowed_shell_scripts"
        );
        assert!(
            !cfg.command.allow_unrestricted_full_access,
            "fresh ToolConfig must not allow unrestricted full-access sessions"
        );
        assert!(
            !cfg.command.bypass_allowlists,
            "fresh ToolConfig must not bypass allowlists"
        );
    }

    #[test]
    fn default_max_read_bytes_is_64k() {
        let cfg = ToolConfig::default();
        assert_eq!(
            cfg.max_read_bytes,
            64 * 1024,
            "default max_read_bytes must stay capped at 64 KB so a single read_file cannot \
             dwarf the proxy per-message envelope; bumping this is a deliberate trade-off and \
             should be paired with a matching change to the agent-loop compaction tier."
        );
    }

    #[test]
    fn dev_loop_config_enables_command_execution() {
        let cfg = ToolConfig::for_autonomous_dev_loop();
        assert!(
            cfg.command.enabled,
            "runtime policy must enable command execution"
        );
        assert!(
            cfg.command.allow_shell,
            "autonomous agents use shell-style run_command payloads"
        );
        assert!(
            cfg.command.binary_allowlist.contains(&"cargo".to_string()),
            "dev-loop verification needs cargo"
        );
        assert!(
            cfg.command.binary_allowlist.contains(&"git".to_string()),
            "workspace synchronization needs git"
        );
        assert!(
            cfg.command
                .binary_allowlist
                .contains(&"powershell".to_string()),
            "Windows PowerShell is used by local agents on Windows"
        );
        assert!(
            !cfg.command.binary_allowlist.is_empty(),
            "enabled command policy must fail open only through an explicit allowlist"
        );
        assert!(
            cfg.command.allow_unrestricted_full_access,
            "autonomous full-access sessions may bypass command allowlists"
        );
    }

    #[cfg(windows)]
    #[test]
    fn dev_loop_binary_allowlist_excludes_ls_on_windows() {
        let cfg = ToolConfig::for_autonomous_dev_loop();
        assert!(
            !cfg.command.binary_allowlist.contains(&"ls".to_string()),
            "Windows dev-loop policy must not advertise an unresolved Unix ls binary"
        );
        assert!(
            cfg.command.binary_allowlist.contains(&"dir".to_string()),
            "Windows dev-loop policy should retain dir as the platform listing command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dev_loop_binary_allowlist_includes_ls_on_unix() {
        let cfg = ToolConfig::for_autonomous_dev_loop();
        assert!(
            cfg.command.binary_allowlist.contains(&"ls".to_string()),
            "Unix dev-loop policy should retain ls"
        );
    }
}
