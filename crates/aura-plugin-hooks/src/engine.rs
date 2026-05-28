//! Hook engine: registration + firing.
//!
//! Phase 4c scope: ships the engine shell + manual fire surface. No
//! caller in the agent loop fires events yet (Phase 8 wires the
//! lifecycle integration). The integration test in
//! `tests/hook_event_smoke.rs` exercises a manual `fire` against a
//! no-op subprocess.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Hook command resolution mirrors the Codex / Claude contract:
//!   - a bare filename (no `/` or `\`) is passed verbatim so the OS
//!     PATH lookup applies — do NOT join with `plugin_root`.
//!   - a relative path WITH a separator joins to `plugin_root`.
//!   - an absolute path is used as-is.
//! - Firing isolates per-hook failures: a spawn or exit-status error
//!   for one hook does not skip the remaining hooks for the same
//!   event. The summary counts per-hook outcomes.
//! - Spawned hooks see an explicitly scrubbed env (see
//!   [`crate::sandbox::scrubbed_inherit`]) plus the
//!   [`crate::HookFiringContext::env_vars`] payload plus any
//!   plugin-author overrides from the registered hook's `env` map.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::context::HookFiringContext;
use crate::error::HookError;
use crate::event::HookEvent;
use crate::sandbox;

/// A single hook registered against an event. Owned by the engine.
#[derive(Clone, Debug)]
pub struct RegisteredHook {
    /// Plugin id that contributed this hook. Used for diagnostics
    /// and to scope `Drop`-time cleanup decisions.
    pub plugin_id: String,
    /// Event this hook listens to.
    pub event: HookEvent,
    /// Command to spawn. See the module-level docs for the path
    /// resolution rules.
    pub command: String,
    /// Command-line arguments. Default empty.
    pub args: Vec<String>,
    /// Cache version directory for the plugin. Used both to inject
    /// `AURA_PLUGIN_ROOT` and to resolve relative `command` paths.
    pub plugin_root: PathBuf,
    /// Plugin-author allowlisted env overrides. Merged into the
    /// spawned process's env after the canonical
    /// [`HookFiringContext::env_vars`] payload.
    pub env: BTreeMap<String, String>,
}

/// Summary of a single [`HookEngine::fire`] call.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookFireSummary {
    /// Number of hooks that returned an exit code of 0.
    pub succeeded: u32,
    /// Number of hooks that failed (spawn error or non-zero exit).
    pub failed: u32,
}

/// Registration + firing surface for lifecycle hooks.
///
/// Phase 4c ships the engine shell only — there is no caller wiring
/// in the agent loop yet. The engine is `Default` constructible and
/// uses interior mutability nowhere; callers wrap it in whatever
/// synchronisation primitive their integration point needs (Phase 8
/// will likely settle on an `Arc<Mutex<HookEngine>>` inside the
/// runtime).
#[derive(Default, Debug)]
pub struct HookEngine {
    by_event: HashMap<HookEvent, Vec<RegisteredHook>>,
}

impl HookEngine {
    /// Construct a new, empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook. Hooks fire in registration order for the
    /// same event.
    pub fn register(&mut self, hook: RegisteredHook) {
        self.by_event.entry(hook.event).or_default().push(hook);
    }

    /// Count of registered hooks for an event. Primarily a test
    /// helper; the firing loop reads the inner table directly.
    #[must_use]
    pub fn registered_count(&self, event: HookEvent) -> usize {
        self.by_event.get(&event).map_or(0, Vec::len)
    }

    /// Fire all hooks registered for [`HookFiringContext::event`] in
    /// registration order.
    ///
    /// Per-hook failures are isolated: the loop continues on error
    /// and the returned summary records succeeded / failed counts.
    /// The engine itself returns `Err` only for engine-level
    /// invariants — Phase 4c has none, so this signature always
    /// returns `Ok` today, but the `Result` is part of the stable
    /// surface for Phase 8 / 9 additions.
    ///
    /// # Errors
    ///
    /// Reserved for future engine-level invariant violations. Phase
    /// 4c always returns `Ok`.
    pub fn fire(&self, ctx: &HookFiringContext) -> Result<HookFireSummary, HookError> {
        let mut summary = HookFireSummary::default();
        let Some(registered) = self.by_event.get(&ctx.event) else {
            return Ok(summary);
        };
        for hook in registered {
            match self.spawn_one(hook, ctx) {
                Ok(()) => summary.succeeded += 1,
                Err(err) => {
                    tracing::warn!(
                        plugin_id = %hook.plugin_id,
                        event = ?hook.event,
                        error = %err,
                        "hook subprocess failed; continuing with remaining hooks"
                    );
                    summary.failed += 1;
                }
            }
        }
        Ok(summary)
    }

    /// Spawn one hook and wait for it. Returns an `Err` for spawn
    /// failures and non-zero exits; the caller (the `fire` loop)
    /// converts each error into a `summary.failed += 1` increment.
    fn spawn_one(&self, hook: &RegisteredHook, ctx: &HookFiringContext) -> Result<(), HookError> {
        let prog = resolve_command(&hook.command, &hook.plugin_root);

        let mut env = sandbox::scrubbed_inherit();
        env.extend(ctx.env_vars());
        for (k, v) in &hook.env {
            env.insert(k.clone(), v.clone());
        }

        let mut cmd = Command::new(prog);
        cmd.args(&hook.args);
        cmd.env_clear();
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let output = cmd.output().map_err(|e| HookError::Spawn {
            plugin_id: hook.plugin_id.clone(),
            event: hook.event,
            source: e,
        })?;
        if !output.status.success() {
            return Err(HookError::ExitFailure {
                plugin_id: hook.plugin_id.clone(),
                event: hook.event,
                code: output.status.code(),
            });
        }
        Ok(())
    }
}

/// Resolve a hook command string against the plugin root.
///
/// See the module-level invariant for the rules:
///
/// - a bare filename (no separator) is passed verbatim so the OS PATH
///   lookup applies.
/// - a relative path WITH a separator joins to `plugin_root`.
/// - an absolute path is used as-is.
fn resolve_command(command: &str, plugin_root: &Path) -> PathBuf {
    let has_separator = command.contains('/') || command.contains('\\');
    let as_path = Path::new(command);
    if as_path.is_absolute() {
        return as_path.to_path_buf();
    }
    if has_separator {
        return plugin_root.join(as_path);
    }
    // Bare filename — let the OS resolve via PATH.
    PathBuf::from(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\plugin\root")
        } else {
            PathBuf::from("/plugin/root")
        }
    }

    #[test]
    fn resolve_bare_filename_does_not_join_root() {
        let r = root();
        let p = resolve_command("python3", &r);
        // Bare name -> just the name (OS PATH lookup happens at spawn time).
        assert_eq!(p, PathBuf::from("python3"));
    }

    #[test]
    fn resolve_relative_path_with_separator_joins_root() {
        let r = root();
        let p = resolve_command("./hooks/pre.sh", &r);
        assert_eq!(p, r.join("./hooks/pre.sh"));
    }

    #[test]
    fn resolve_relative_subdir_path_joins_root() {
        let r = root();
        let p = resolve_command("hooks/pre.sh", &r);
        assert_eq!(p, r.join("hooks/pre.sh"));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_windows_backslash_path_joins_root() {
        let r = root();
        let p = resolve_command(r"hooks\pre.cmd", &r);
        assert_eq!(p, r.join(r"hooks\pre.cmd"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_absolute_path_used_as_is_unix() {
        let r = root();
        let p = resolve_command("/usr/bin/true", &r);
        assert_eq!(p, PathBuf::from("/usr/bin/true"));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_absolute_path_used_as_is_windows() {
        let r = root();
        let p = resolve_command(r"C:\Windows\System32\cmd.exe", &r);
        assert_eq!(p, PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    }

    #[test]
    fn register_buckets_by_event() {
        let mut engine = HookEngine::default();
        engine.register(RegisteredHook {
            plugin_id: "p1".into(),
            event: HookEvent::SessionStart,
            command: "noop".into(),
            args: vec![],
            plugin_root: root(),
            env: BTreeMap::new(),
        });
        engine.register(RegisteredHook {
            plugin_id: "p2".into(),
            event: HookEvent::SessionStart,
            command: "noop".into(),
            args: vec![],
            plugin_root: root(),
            env: BTreeMap::new(),
        });
        engine.register(RegisteredHook {
            plugin_id: "p3".into(),
            event: HookEvent::Stop,
            command: "noop".into(),
            args: vec![],
            plugin_root: root(),
            env: BTreeMap::new(),
        });
        assert_eq!(engine.registered_count(HookEvent::SessionStart), 2);
        assert_eq!(engine.registered_count(HookEvent::Stop), 1);
        assert_eq!(engine.registered_count(HookEvent::PreToolUse), 0);
    }
}
