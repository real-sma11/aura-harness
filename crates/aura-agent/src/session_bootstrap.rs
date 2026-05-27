//! Shared session-bootstrap helpers for `aura-runtime`, the TUI harness
//! and any future embedder.
//!
//! Phase 3 consolidated the per-binary copies of
//! `default_agent_config` and `build_executor_router_with_config`
//! into this module so the TUI and the headless node can't silently
//! drift on executor wiring. The TUI-side file
//! (`src/session_helpers.rs`) is now a thin `pub use` re-export layer.

use crate::AgentLoopConfig;
use aura_kernel::ExecutorRouter;
use aura_prompts::default_system_prompt;
use aura_reasoner::ToolDefinition;
use aura_store::RocksStore;
use aura_tools::{ToolCatalog, ToolConfig, ToolExecutor};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolve the canonical store path, migrating from legacy `store/` if needed.
///
/// Canonical path: `{data_dir}/db`. If a legacy `{data_dir}/store` directory
/// exists and the canonical one does not, performs a one-time rename migration.
/// If both exist, the legacy directory is automatically removed.
pub fn resolve_store_path(data_dir: &Path) -> PathBuf {
    let canonical = data_dir.join("db");
    let legacy = data_dir.join("store");

    if canonical.exists() {
        if legacy.exists() {
            match std::fs::remove_dir_all(&legacy) {
                Ok(()) => {
                    tracing::info!(
                        legacy = %legacy.display(),
                        "Removed stale legacy 'store' directory"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        legacy = %legacy.display(),
                        "Failed to remove legacy 'store' directory — please remove it manually"
                    );
                }
            }
        }
        return canonical;
    }
    if legacy.exists() {
        match std::fs::rename(&legacy, &canonical) {
            Ok(()) => {
                tracing::info!(
                    from = %legacy.display(),
                    to = %canonical.display(),
                    "Migrated store from legacy path to canonical path"
                );
                return canonical;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    legacy = %legacy.display(),
                    "Failed to migrate store — falling back to legacy path"
                );
                return legacy;
            }
        }
    }
    canonical
}

pub fn open_store(path: &Path) -> anyhow::Result<Arc<RocksStore>> {
    Ok(Arc::new(RocksStore::open(path, false)?))
}

/// Build the default executor router used by the terminal harness and
/// embedded tooling.
///
/// **Phase 5 hardening note:** This wires in
/// [`ToolExecutor::with_defaults()`], which — after the Phase 5 flip of
/// [`aura_tools::ToolConfig::default`] — is a *no-shell, no-commands*
/// tool router. Filesystem tools (`read_file`, `write_file`, `list_files`,
/// …) are reachable, but `run_command` is blocked at `CmdRunTool::execute`
/// (`command.enabled = false` and an empty `binary_allowlist`).
///
/// Production callers that want command execution must *not* rely on
/// this helper. They should construct a custom
/// [`aura_tools::ToolConfig`] with `command.enabled: true` and a
/// populated `binary_allowlist`, feed it into [`ToolExecutor::new`],
/// and register that executor on their own `ExecutorRouter`. The opt-in
/// is deliberately plumbed through user-supplied config rather than a
/// convenience flag on this bootstrap.
#[must_use]
pub fn build_executor_router() -> (ExecutorRouter, Vec<ToolDefinition>) {
    let mut executor_router = ExecutorRouter::new();
    executor_router.add_executor(Arc::new(ToolExecutor::with_defaults()));

    let tools = ToolCatalog::new().executor_builtin_tools();

    (executor_router, tools)
}

#[must_use]
pub fn load_auth_token() -> Option<String> {
    std::env::var("AURA_ROUTER_JWT")
        .ok()
        .or_else(aura_auth::CredentialStore::load_token)
}

// `ProviderSelection` / `select_provider` were removed in Wave 4. The
// canonical factory now lives in [`aura_reasoner::provider_factory`].
// Callers use `aura_reasoner::default_provider_from_env`,
// `aura_reasoner::with_session_overrides`, and
// `aura_reasoner::mock_provider`.

// ---------------------------------------------------------------------
// Phase 3 consolidation: moved from `src/session_helpers.rs`.
//
// These helpers used to live next to the TUI binary but were needed by
// `aura-runtime` and future embedders too. The remaining bootstrap helpers
// are configuration-identical across deploys; per-tool enablement is policy
// state, not environment.
// ---------------------------------------------------------------------

/// Default [`AgentLoopConfig`] used by the TUI and other CLI-shaped
/// embedders — pulls the canonical system prompt and the harness auth
/// token, leaves everything else at the [`AgentLoopConfig::for_agent`]
/// defaults.
///
/// Callers must thread their model selection through here. The TUI uses
/// [`aura_reasoner::ENV_FALLBACK_MODEL`] (the seed of the reasoner's
/// `AURA_DEFAULT_MODEL` env-var fallback). Higher-level surfaces — the
/// chat WS path, the dev-loop / task-run automatons — pin the
/// user-selected model and never fall through to the env seed.
#[must_use]
pub fn default_agent_config(model: impl Into<String>) -> AgentLoopConfig {
    AgentLoopConfig {
        system_prompt: default_system_prompt(),
        auth_token: load_auth_token(),
        ..AgentLoopConfig::for_agent(model)
    }
}

/// Build an executor router honoring a caller-supplied [`ToolConfig`].
///
/// The plain [`build_executor_router`] hard-codes
/// `ToolExecutor::with_defaults()`. This variant threads explicit runtime
/// execution policy through [`ToolExecutor::new`].
#[must_use]
pub fn build_executor_router_with_config(
    tool_config: &ToolConfig,
) -> (ExecutorRouter, Vec<ToolDefinition>) {
    let mut executor_router = ExecutorRouter::new();
    executor_router.add_executor(Arc::new(ToolExecutor::new(tool_config.clone())));

    let tools = ToolCatalog::new().executor_builtin_tools();

    (executor_router, tools)
}
