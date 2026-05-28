//! CLI argument definitions and parsing.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aura",
    about = "AURA CLI - Autonomous Universal Reasoning Architecture"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the agent (default when no subcommand is given).
    Run(RunArgs),
    /// Authenticate with zOS to obtain a JWT for proxy mode.
    Login,
    /// Clear stored authentication credentials.
    Logout,
    /// Show current authentication status.
    Whoami,
    /// Print "Hello, World!" and exit (Spec 01).
    Hello,
    /// Phase 4a stub: migrate aura state (no-op today).
    ///
    /// Future phases will populate this with Codex → Aura state
    /// migration. The stub exists so the CLI surface is stable when
    /// `aura-store-db` and `aura-plugin-core` (Phase 4b+) start
    /// producing migration-aware on-disk layouts.
    Migrate(MigrateArgs),
    /// Manage declarative plugins (Phase 4b).
    ///
    /// Subcommands install / list / enable / disable interact with
    /// the on-disk plugin cache under `AURA_HOME/plugins/` and the
    /// `[plugins.<id>]` table inside `AURA_HOME/config.toml`. No
    /// agent-loop wiring lands until Phase 4c (hooks/MCP/connectors)
    /// and Phase 8 (full integration).
    Plugins(PluginsCommand),
}

/// Arguments for the `migrate` subcommand (Phase 4a stub).
#[derive(Args, Debug)]
pub struct MigrateArgs {
    /// Run as a dry preview without making changes.
    #[arg(long)]
    pub dry_run: bool,
}

/// `aura plugins` parent command (Phase 4b).
#[derive(Args, Debug)]
pub struct PluginsCommand {
    /// Plugins subcommand to run.
    #[command(subcommand)]
    pub action: PluginsSubcommand,
}

/// Subcommands under `aura plugins` (Phase 4b).
#[derive(Subcommand, Debug)]
pub enum PluginsSubcommand {
    /// Install a plugin from a local source directory.
    ///
    /// The source must contain a `.aura-plugin/`, `.codex-plugin/`,
    /// or `.claude-plugin/` subdirectory with a `manifest.toml`. The
    /// install pipeline copies the source tree into the
    /// `AURA_HOME/plugins/<id>/<version>/` cache layout and writes a
    /// normalised `.aura-plugin.toml` regardless of which alias the
    /// source used.
    Install {
        /// Path to the plugin source directory.
        source: PathBuf,
        /// Bypass `trust.require_explicit_trust = true` manifests.
        #[arg(long)]
        trust: bool,
    },
    /// List installed plugins and their active versions.
    List,
    /// Enable a plugin in `AURA_HOME/config.toml` (`enabled = true`).
    ///
    /// When the cached manifest has `trust.require_explicit_trust = true`
    /// and the plugin is not already trusted in operator config, the
    /// flow prompts the operator on the TTY. `--yes` skips the
    /// prompt and accepts trust; `--no` skips the prompt and
    /// declines (the plugin is NOT enabled).
    Enable {
        /// Plugin id (matches the manifest `id` field).
        id: String,
        /// Bypass the trust prompt and accept trust automatically.
        /// Mutually exclusive with `--no`.
        #[arg(long, conflicts_with = "no")]
        yes: bool,
        /// Bypass the trust prompt and decline trust automatically.
        /// Mutually exclusive with `--yes`.
        #[arg(long, conflicts_with = "yes")]
        no: bool,
    },
    /// Disable a plugin in `AURA_HOME/config.toml` (`enabled = false`).
    Disable {
        /// Plugin id (matches the manifest `id` field).
        id: String,
    },
}

/// Arguments for the `run` subcommand (also the default behaviour).
#[derive(Parser)]
pub struct RunArgs {
    /// UI mode (terminal or none)
    #[arg(long, default_value = "terminal")]
    pub ui: UiMode,

    /// Theme (cyber, matrix, synthwave, minimal)
    #[arg(long, default_value = "cyber")]
    pub theme: String,

    /// Working directory
    #[arg(short, long)]
    pub dir: Option<PathBuf>,

    /// Model provider (anthropic or mock)
    #[arg(long, default_value = "anthropic")]
    pub provider: String,

    /// Enable verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Permit FullAccess sessions to bypass command allowlists.
    #[arg(long)]
    pub allow_unrestricted_full_access: bool,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            ui: UiMode::Terminal,
            theme: "cyber".to_string(),
            dir: None,
            provider: "anthropic".to_string(),
            verbose: false,
            allow_unrestricted_full_access: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UiMode {
    /// Full terminal UI (default)
    Terminal,
    /// No UI, run as swarm server
    None,
}
