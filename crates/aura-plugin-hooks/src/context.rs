//! Per-firing context handed to a hook command.
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`HookFiringContext::env_vars`] always emits the canonical Aura
//!   variables (`AURA_PLUGIN_ROOT`, `AURA_EVENT`, `AURA_AGENT_ID`,
//!   `AURA_SESSION_ID`) and the Codex / Claude compatibility aliases
//!   (`CODEX_PLUGIN_ROOT`, `CLAUDE_PLUGIN_ROOT`). The aliases are
//!   read-only by spec — plugin authors targeting either ecosystem
//!   can pick whichever name their existing scripts expect.
//! - The `extra` map is merged last so plugin-author overrides can
//!   shadow canonical names if the manifest explicitly opts in. We
//!   accept that trade-off in exchange for not maintaining a denylist
//!   here; the manifest schema validator owns that policy.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aura_core::AgentId;

use crate::event::HookEvent;

/// Per-firing context handed to a hook command.
///
/// Carries the plugin root (so the spawned process can resolve its
/// own files), the event being fired, and the agent / session / turn
/// identifiers needed by Codex / Claude-compatible hook scripts.
#[derive(Clone, Debug)]
pub struct HookFiringContext {
    /// Cache version directory for the plugin firing the hook. The
    /// spawned process should resolve its bundled files relative to
    /// this path (the `AURA_PLUGIN_ROOT` env var carries the same
    /// value).
    pub plugin_root: PathBuf,
    /// Lifecycle event being fired.
    pub event: HookEvent,
    /// Agent id firing the hook.
    pub agent_id: AgentId,
    /// Session id firing the hook.
    pub session_id: String,
    /// Optional turn id when the event happens inside a turn. `None`
    /// for `SessionStart` / `Stop` / similar lifecycle events.
    pub turn_id: Option<String>,
    /// Free-form extra variables the firing site wants injected. The
    /// keys are merged last and shadow canonical names if collisions
    /// occur (see module-level invariant note).
    pub extra: BTreeMap<String, String>,
}

impl HookFiringContext {
    /// Build the env-var set to inject into the spawned hook process.
    ///
    /// Includes:
    ///
    /// - `AURA_PLUGIN_ROOT`, `AURA_EVENT`, `AURA_AGENT_ID`,
    ///   `AURA_SESSION_ID` (canonical).
    /// - `AURA_TURN_ID` when [`Self::turn_id`] is `Some`.
    /// - `CODEX_PLUGIN_ROOT` and `CLAUDE_PLUGIN_ROOT` (read-only
    ///   compatibility aliases that mirror `AURA_PLUGIN_ROOT`).
    /// - Every entry in [`Self::extra`], merged last.
    #[must_use]
    pub fn env_vars(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        let plugin_root = self.plugin_root.to_string_lossy().into_owned();

        env.insert("AURA_PLUGIN_ROOT".to_string(), plugin_root.clone());
        env.insert("AURA_EVENT".to_string(), self.event.as_str().to_string());
        env.insert("AURA_AGENT_ID".to_string(), self.agent_id.to_string());
        env.insert("AURA_SESSION_ID".to_string(), self.session_id.clone());
        if let Some(t) = &self.turn_id {
            env.insert("AURA_TURN_ID".to_string(), t.clone());
        }

        env.insert("CODEX_PLUGIN_ROOT".to_string(), plugin_root.clone());
        env.insert("CLAUDE_PLUGIN_ROOT".to_string(), plugin_root);

        for (k, v) in &self.extra {
            env.insert(k.clone(), v.clone());
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_agent_id() -> AgentId {
        AgentId::new([0xABu8; 32])
    }

    #[test]
    fn env_vars_include_canonical_set() {
        let ctx = HookFiringContext {
            plugin_root: PathBuf::from("/tmp/plug"),
            event: HookEvent::SessionStart,
            agent_id: sample_agent_id(),
            session_id: "sess-1".into(),
            turn_id: None,
            extra: BTreeMap::new(),
        };
        let env = ctx.env_vars();
        assert!(env.contains_key("AURA_PLUGIN_ROOT"));
        assert!(env.contains_key("AURA_EVENT"));
        assert!(env.contains_key("AURA_AGENT_ID"));
        assert!(env.contains_key("AURA_SESSION_ID"));
        assert_eq!(
            env.get("AURA_EVENT").map(String::as_str),
            Some("session_start")
        );
    }

    #[test]
    fn env_vars_include_codex_and_claude_aliases() {
        let ctx = HookFiringContext {
            plugin_root: PathBuf::from("/tmp/plug"),
            event: HookEvent::PreToolUse,
            agent_id: sample_agent_id(),
            session_id: "sess-2".into(),
            turn_id: Some("turn-1".into()),
            extra: BTreeMap::new(),
        };
        let env = ctx.env_vars();
        let canonical = env
            .get("AURA_PLUGIN_ROOT")
            .expect("canonical plugin root must be present");
        assert_eq!(env.get("CODEX_PLUGIN_ROOT"), Some(canonical));
        assert_eq!(env.get("CLAUDE_PLUGIN_ROOT"), Some(canonical));
        assert_eq!(env.get("AURA_TURN_ID").map(String::as_str), Some("turn-1"));
    }

    #[test]
    fn env_vars_omit_turn_id_when_none() {
        let ctx = HookFiringContext {
            plugin_root: PathBuf::from("/tmp/plug"),
            event: HookEvent::Stop,
            agent_id: sample_agent_id(),
            session_id: "sess-3".into(),
            turn_id: None,
            extra: BTreeMap::new(),
        };
        let env = ctx.env_vars();
        assert!(!env.contains_key("AURA_TURN_ID"));
    }

    #[test]
    fn extra_entries_are_merged_last() {
        let mut extra = BTreeMap::new();
        extra.insert("MY_PLUGIN_FLAG".to_string(), "yes".to_string());
        let ctx = HookFiringContext {
            plugin_root: PathBuf::from("/tmp/plug"),
            event: HookEvent::PostToolUse,
            agent_id: sample_agent_id(),
            session_id: "sess-4".into(),
            turn_id: None,
            extra,
        };
        let env = ctx.env_vars();
        assert_eq!(env.get("MY_PLUGIN_FLAG").map(String::as_str), Some("yes"));
    }
}
