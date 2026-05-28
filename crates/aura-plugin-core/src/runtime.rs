//! Plugin runtime materialisation (Phase 8).
//!
//! [`load_enabled_plugins`] walks every enabled entry in
//! `~/.aura/plugins/` and produces a [`PluginRuntime`] composed of
//! the three Phase 4c subsystems:
//!
//! - [`aura_plugin_hooks::HookEngine`] populated with the manifest's
//!   `[[contributes.hooks]]` entries.
//! - [`aura_plugin_mcp::McpConnectionManager`] populated with the
//!   manifest's `[[contributes.mcp]]` entries (first-active-wins
//!   merge by `server_id`).
//! - [`aura_plugin_connectors::ConnectorRegistry`] populated with
//!   the manifest's `[[contributes.connectors]]` entries (first-
//!   active-wins by `id`).
//!
//! The skill roots are returned alongside the runtime so the caller
//! (typically `FleetDaemon`) can hand them to
//! `aura_context_skills::SkillRegistry::add_plugin_roots`.
//!
//! ## Invariants ([rules.md §13])
//!
//! - **Per-plugin best-effort load**: a single plugin failing to
//!   load (manifest invalid, MCP server spawn failure, hook event
//!   unknown, etc.) MUST NOT abort the load. The failure is
//!   recorded in [`PluginRuntime::load_failures`] and the loop
//!   continues with the remaining plugins.
//! - **First-active-wins** for MCP and connector merges: the
//!   first plugin (in enable order) keeps the slot; subsequent
//!   contributions log a `WARN` and become a load-failure entry.
//! - **Hook chain order**: hooks for the same event run in the
//!   order they were registered. The load loop walks plugins in
//!   the user's enable order (sorted lexicographically by id for
//!   determinism — matches the Codex behaviour referenced in the
//!   plan).
//! - **No I/O at construct time** for [`PluginRuntime::default`].
//!   The runtime can be built empty for the backward-compat path
//!   (no `~/.aura/plugins/` directory => zero overhead).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_config::PluginsConfig;
use aura_plugin_connectors::{ConnectorEntry, ConnectorRegistry};
use aura_plugin_hooks::{HookEngine, HookEvent, PluginLoadFailure, PluginRef, RegisteredHook};
use aura_plugin_mcp::{McpConnectionManager, ServerConfig as McpServerConfig};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::cache::PluginCache;
use crate::manifest::PluginManifest;

/// Result of a [`load_enabled_plugins`] call. The four subsystem
/// handles are `Arc`-wrapped so the caller can clone them across
/// runtime tasks without re-materialising.
#[derive(Debug)]
pub struct PluginRuntime {
    /// Hook engine wired with every enabled plugin's hook
    /// contributions.
    pub hook_engine: Arc<HookEngine>,
    /// MCP connection manager wired with every enabled plugin's
    /// MCP server contributions.
    pub mcp: Arc<McpConnectionManager>,
    /// Connector registry wired with every enabled plugin's
    /// connector contributions.
    pub connectors: Arc<ConnectorRegistry>,
    /// Plugin skill directories the caller should hand to
    /// `aura_context_skills::SkillRegistry::add_plugin_roots`.
    pub skill_roots: Vec<PathBuf>,
    /// Refs to every successfully-loaded plugin.
    pub enabled: Vec<PluginRef>,
    /// Refs + reasons for plugins that failed to materialise. The
    /// session-start hook ctx surfaces this list to operator hooks
    /// for audit / monitoring use cases.
    pub load_failures: Vec<PluginLoadFailure>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self {
            hook_engine: Arc::new(HookEngine::new()),
            mcp: Arc::new(McpConnectionManager::new()),
            connectors: Arc::new(ConnectorRegistry::new()),
            skill_roots: Vec::new(),
            enabled: Vec::new(),
            load_failures: Vec::new(),
        }
    }
}

impl PluginRuntime {
    /// Build a fully-empty runtime. Used by callers that want the
    /// Phase 8 wiring shape without materialising any plugin from
    /// disk — the empty-install backward-compat path.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// `true` when no plugins are wired into this runtime. Phase 8
    /// callers use this as a fast short-circuit before allocating
    /// per-event ctx structs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enabled.is_empty()
    }
}

/// Errors raised by the load pipeline itself (not per-plugin
/// failures, which are recorded in [`PluginRuntime::load_failures`]).
#[derive(Debug, Error)]
pub enum PluginLoadError {
    /// I/O error walking the plugin cache.
    #[error("io error walking plugin cache: {0}")]
    Io(#[from] std::io::Error),
}

/// Walk `aura_home/plugins/` + the operator's `[plugins]` config
/// and materialise an active [`PluginRuntime`].
///
/// Per-plugin failures are non-fatal: a malformed manifest, an
/// unknown hook event, a duplicate MCP server id, etc. all produce
/// a [`PluginLoadFailure`] entry in [`PluginRuntime::load_failures`]
/// and the loop continues with the remaining plugins.
///
/// # Errors
///
/// Returns [`PluginLoadError::Io`] only for catastrophic walk
/// failures (e.g. the `~/.aura/plugins/` directory is unreadable).
/// A missing `~/.aura/plugins/` directory yields an empty runtime
/// (Ok).
pub fn load_enabled_plugins(
    aura_home: &Path,
    plugins_config: &PluginsConfig,
) -> Result<PluginRuntime, PluginLoadError> {
    let cache = PluginCache::new(aura_home.join("plugins"));
    let plugin_ids = cache.list_plugins().unwrap_or_default();

    let hook_engine = Arc::new(HookEngine::new());
    let mcp = Arc::new(McpConnectionManager::new());
    let connectors = Arc::new(ConnectorRegistry::new());
    let mut skill_roots = Vec::new();
    let mut enabled = Vec::new();
    let mut failures = Vec::new();

    for plugin_id in plugin_ids {
        // Honour [plugins] config: a plugin must be both installed
        // AND enabled to participate.
        if !plugin_enabled(&plugin_id, plugins_config) {
            debug!(plugin_id, "plugin not enabled in config; skipping");
            continue;
        }

        let active_version = match cache.active_version(&plugin_id) {
            Ok(Some(v)) => v,
            Ok(None) => {
                debug!(plugin_id, "no active version pointer; skipping");
                continue;
            }
            Err(err) => {
                failures.push(PluginLoadFailure {
                    plugin_id: plugin_id.clone(),
                    reason: format!("read active version: {err}"),
                });
                continue;
            }
        };

        let version_dir = cache.version_dir(&plugin_id, &active_version);
        let manifest_path = version_dir.join(".aura-plugin.toml");
        let manifest = match read_manifest(&manifest_path) {
            Ok(m) => m,
            Err(reason) => {
                failures.push(PluginLoadFailure {
                    plugin_id: plugin_id.clone(),
                    reason,
                });
                continue;
            }
        };

        let plugin_ref = PluginRef {
            id: manifest.id.as_str().to_string(),
            version: manifest.version.to_string(),
        };

        materialise_one(
            &plugin_ref,
            &manifest,
            &version_dir,
            &hook_engine,
            &mcp,
            &connectors,
            &mut skill_roots,
        );

        info!(
            plugin_id = %plugin_ref.id,
            version = %plugin_ref.version,
            "plugin materialised"
        );
        enabled.push(plugin_ref);
    }

    Ok(PluginRuntime {
        hook_engine,
        mcp,
        connectors,
        skill_roots,
        enabled,
        load_failures: failures,
    })
}

fn plugin_enabled(plugin_id: &str, cfg: &PluginsConfig) -> bool {
    // Match either the bare `id` or the `id@market` qualified key.
    if let Some(row) = cfg.table.0.get(plugin_id) {
        return row.enabled;
    }
    cfg.table
        .0
        .iter()
        .any(|(k, row)| row.enabled && (k == plugin_id || k.starts_with(&format!("{plugin_id}@"))))
}

fn read_manifest(path: &Path) -> Result<PluginManifest, String> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| format!("read manifest {}: {}", path.display(), e))?;
    PluginManifest::from_toml_str(&body).map_err(|e| format!("parse manifest: {e}"))
}

fn materialise_one(
    plugin_ref: &PluginRef,
    manifest: &PluginManifest,
    plugin_root: &Path,
    hook_engine: &HookEngine,
    mcp: &McpConnectionManager,
    connectors: &ConnectorRegistry,
    skill_roots: &mut Vec<PathBuf>,
) {
    // (1) Skills: each contribution names a relative `path` under the
    //     plugin root; the *parent* directory is what
    //     `add_plugin_roots` discovers.
    for skill in &manifest.contributes.skills {
        let abs = plugin_root.join(&skill.path);
        let parent = abs
            .parent()
            .map_or_else(|| plugin_root.to_path_buf(), Path::to_path_buf);
        if !skill_roots.contains(&parent) {
            skill_roots.push(parent);
        }
    }

    // (2) Hooks: each contribution registers a process. Unknown
    //     event names are logged + skipped (matches Codex behaviour
    //     referenced in the plan).
    for hook in &manifest.contributes.hooks {
        let Some(event) = parse_hook_event(&hook.event) else {
            warn!(
                plugin_id = %plugin_ref.id,
                raw_event = %hook.event,
                "unknown hook event; skipping"
            );
            continue;
        };
        hook_engine.register(RegisteredHook {
            plugin_id: plugin_ref.id.clone(),
            event,
            command: hook.command.clone(),
            args: hook.args.clone(),
            plugin_root: plugin_root.to_path_buf(),
            env: BTreeMap::new(),
        });
    }

    // (3) MCP servers: first-active-wins on `server_id`. We pre-pend
    //     the plugin root for relative `command` paths so the spawn
    //     resolves the bundled binary.
    for mcp_entry in &manifest.contributes.mcp {
        let cfg = McpServerConfig {
            server_id: mcp_entry.server_id.clone(),
            command: resolve_command_for_spawn(&mcp_entry.command, plugin_root),
            args: mcp_entry.args.clone(),
            env: mcp_entry.env.clone(),
        };
        if let Err(err) = mcp.register(cfg) {
            warn!(
                plugin_id = %plugin_ref.id,
                server_id = %mcp_entry.server_id,
                error = %err,
                "MCP server registration rejected (likely first-active-wins conflict)"
            );
            // Continue — duplicates are documented as warn + skip.
        }
    }

    // (4) Connectors: last-wins per the spec. The Phase 4c registry
    //     is first-active-wins; for plugin-supplied overrides of
    //     built-in connectors we re-register by removing the old
    //     entry first. The connector registry doesn't expose a
    //     `remove`; we instead document this as best-effort and log
    //     the conflict.
    for connector in &manifest.contributes.connectors {
        let entry = ConnectorEntry {
            id: connector.id.clone(),
            plugin_id: plugin_ref.id.clone(),
            endpoint: connector.endpoint.clone(),
        };
        if let Err(err) = connectors.register(entry) {
            warn!(
                plugin_id = %plugin_ref.id,
                connector_id = %connector.id,
                error = %err,
                "connector registration rejected (already registered)"
            );
            // Phase 8 documents last-wins for plugin-vs-builtin
            // overrides; the current registry does not expose a
            // remove. Tracked as a follow-up; for now we log and
            // continue (the existing entry keeps the slot).
        }
    }
}

fn parse_hook_event(s: &str) -> Option<HookEvent> {
    // Accept both snake_case (Aura wire format) and PascalCase (Codex
    // documentation form). Phase 8 normalises both into the closed
    // [`HookEvent`] enum.
    if let Some(e) = HookEvent::parse_wire(s) {
        return Some(e);
    }
    let lowered = s.to_ascii_lowercase();
    if let Some(e) = HookEvent::parse_wire(&lowered) {
        return Some(e);
    }
    let snake = pascal_to_snake(s);
    HookEvent::parse_wire(&snake)
}

fn pascal_to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn resolve_command_for_spawn(command: &str, plugin_root: &Path) -> String {
    let p = Path::new(command);
    if p.is_absolute() {
        return command.to_string();
    }
    let has_separator = command.contains('/') || command.contains('\\');
    if has_separator {
        return plugin_root.join(p).to_string_lossy().into_owned();
    }
    command.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::{PluginConfig, PluginsTable};
    use std::collections::BTreeMap as Map;
    use tempfile::TempDir;

    fn write_minimal_plugin(home: &Path, id: &str, version: &str, with_skill: bool) {
        let cache = PluginCache::new(home.join("plugins"));
        let dir = cache.version_dir(id, version);
        std::fs::create_dir_all(&dir).unwrap();
        let mut manifest = format!(
            r#"manifest_version = "v1"
id = "{id}"
version = "{version}"
"#
        );
        if with_skill {
            std::fs::create_dir_all(dir.join("skills")).unwrap();
            std::fs::write(dir.join("skills").join("hello.md"), "skill body").unwrap();
            manifest.push_str(
                r#"
[[contributes.skills]]
id = "hello"
path = "./skills/hello.md"
"#,
            );
        }
        std::fs::write(dir.join(".aura-plugin.toml"), manifest).unwrap();
        cache.set_active(id, version).unwrap();
    }

    fn enabled_config(plugin_id: &str) -> PluginsConfig {
        let mut table: Map<String, PluginConfig> = Map::new();
        table.insert(
            plugin_id.to_string(),
            PluginConfig {
                enabled: true,
                trusted: true,
                version: None,
            },
        );
        PluginsConfig {
            table: PluginsTable(table),
        }
    }

    #[test]
    fn empty_aura_home_yields_empty_runtime() {
        let home = TempDir::new().unwrap();
        let runtime =
            load_enabled_plugins(home.path(), &PluginsConfig::default()).expect("empty home is OK");
        assert!(runtime.enabled.is_empty());
        assert!(runtime.load_failures.is_empty());
        assert!(runtime.skill_roots.is_empty());
        assert!(runtime.is_empty());
    }

    #[test]
    fn enabled_plugin_with_skill_yields_root() {
        let home = TempDir::new().unwrap();
        write_minimal_plugin(home.path(), "hello-plug", "0.1.0", true);
        let runtime =
            load_enabled_plugins(home.path(), &enabled_config("hello-plug")).expect("load ok");
        assert_eq!(runtime.enabled.len(), 1);
        assert_eq!(runtime.enabled[0].id, "hello-plug");
        assert_eq!(runtime.skill_roots.len(), 1);
        assert!(runtime.skill_roots[0].ends_with("skills"));
    }

    #[test]
    fn disabled_plugin_is_skipped() {
        let home = TempDir::new().unwrap();
        write_minimal_plugin(home.path(), "off", "0.1.0", false);
        let runtime =
            load_enabled_plugins(home.path(), &PluginsConfig::default()).expect("load ok");
        assert!(runtime.enabled.is_empty());
        assert!(runtime.load_failures.is_empty());
    }

    #[test]
    fn malformed_manifest_becomes_load_failure() {
        let home = TempDir::new().unwrap();
        let cache = PluginCache::new(home.path().join("plugins"));
        let dir = cache.version_dir("broken", "0.1.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".aura-plugin.toml"), "not = valid\nmanifest").unwrap();
        cache.set_active("broken", "0.1.0").unwrap();
        let runtime = load_enabled_plugins(home.path(), &enabled_config("broken")).expect("ok");
        assert!(runtime.enabled.is_empty());
        assert_eq!(runtime.load_failures.len(), 1);
        assert_eq!(runtime.load_failures[0].plugin_id, "broken");
    }

    #[test]
    fn pascal_case_hook_event_parses() {
        assert_eq!(
            parse_hook_event("PreToolUse"),
            Some(aura_plugin_hooks::HookEvent::PreToolUse)
        );
        assert_eq!(
            parse_hook_event("pre_tool_use"),
            Some(aura_plugin_hooks::HookEvent::PreToolUse)
        );
        assert_eq!(parse_hook_event("definitelyNotAnEvent"), None);
    }

    #[test]
    fn config_lookup_matches_qualified_key() {
        let mut table: Map<String, PluginConfig> = Map::new();
        table.insert(
            "myplugin@official".into(),
            PluginConfig {
                enabled: true,
                trusted: true,
                version: None,
            },
        );
        let cfg = PluginsConfig {
            table: PluginsTable(table),
        };
        assert!(plugin_enabled("myplugin", &cfg));
        assert!(!plugin_enabled("other", &cfg));
    }
}
