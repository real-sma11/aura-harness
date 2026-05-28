//! # aura-plugin-api
//!
//! Layer: plugin
//!
//! In-process contributor trait surface for first-party plugins.
//!
//! **Scope** (Phase 4b): This crate is NOT a dynamic plugin loader.
//! It defines the Rust trait surface that first-party plugins compile
//! against. On-disk plugin manifests (`.aura-plugin`, `.codex-plugin`,
//! `.claude-plugin`) flow through `aura-plugin-core` and contribute
//! via the runtime integration crates landing in Phase 4c
//! (`aura-plugin-hooks`, `aura-plugin-mcp`, `aura-plugin-connectors`).
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`PluginId`] is a newtype around `String` per rules §5 — no
//!   raw-string identity in agent code.
//! - [`ContributionKind`] is a **closed enum** matching the
//!   aura-core-modes invariant: adding a variant is a breaking change
//!   for downstream contributors. The `closed_enum_invariant` test in
//!   this file deliberately matches every variant without a `_`
//!   wildcard so the compiler catches a missed case.
//! - This crate has zero `aura-*` deps. It sits at the bottom of the
//!   plugin layer and is consumed by `aura-plugin-core` for shared
//!   identity types.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::path::PathBuf;

use thiserror::Error;

/// A first-party plugin identity. Newtype around `String` per rules §5.
///
/// V1 keeps the identity flat (just a string). The richer
/// `name@marketplace` form lives in `aura-plugin-core::PluginId`
/// when the marketplace pipeline lands — for the first-party
/// contributor API there is no marketplace, only the plugin's own
/// chosen id.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PluginId(String);

impl PluginId {
    /// Construct a new [`PluginId`] from any string-like input.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Categories of contribution a plugin can make.
///
/// **Closed-enum invariant** ([rules.md §13]): downstream consumers
/// cannot extend this enum. The
/// [`tests::closed_enum_invariant`] test matches every variant
/// without a `_` wildcard so adding a variant breaks compilation in
/// the trait surface as well — that's deliberate.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContributionKind {
    /// Static skill (system-prompt-attached doc).
    Skill,
    /// Lifecycle hook (subprocess invoked on hook events).
    Hook,
    /// Model Context Protocol server contribution.
    Mcp,
    /// External connector / tool integration.
    Connector,
    /// System-prompt addendum.
    SystemPrompt,
    /// Slash-command / CLI command.
    Command,
    /// Subagent recipe.
    Agent,
}

/// Errors a [`PluginContributor`] implementation can surface.
#[derive(Debug, Error)]
pub enum PluginApiError {
    /// Contributor-supplied error message. Wrapped here so the
    /// surface stays a single thiserror enum without leaking the
    /// contributor's internal error type into this crate's public
    /// API.
    #[error("contributor error: {0}")]
    Contributor(String),
}

/// In-process contributor trait. First-party crates implement this to
/// register skills, hooks, MCP, etc. at build time. NOT a dynamic
/// loader — `dlopen`-style loading is out of scope for V1
/// ([plan §6 plugin trust]).
pub trait PluginContributor: Send + Sync {
    /// Stable identifier for this contributor — surfaces in trace
    /// output, registry diagnostics, and the future
    /// `aura plugins doctor` command.
    fn plugin_id(&self) -> &PluginId;
    /// Categories of contribution this contributor exposes. Used by
    /// the future `ExtensionRegistry` to validate that a contributor
    /// can fulfil the slots it claims.
    fn contributions(&self) -> Vec<ContributionKind>;
}

/// Root location of a materialised on-disk plugin payload. Owned by
/// `aura-plugin-core`'s install pipeline; exposed here so first-party
/// contributors that bridge into the on-disk plugin world (e.g. the
/// future skills materialiser) can reference the cache without
/// pulling a hard dep on `aura-plugin-core`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginRoot {
    /// Absolute path to the plugin payload root (cache version dir).
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_round_trip() {
        let id = PluginId::new("alpha");
        assert_eq!(id.as_str(), "alpha");
        let id2 = PluginId::new(String::from("beta"));
        assert_eq!(id2.as_str(), "beta");
        let id3 = PluginId::new("gamma");
        assert_eq!(format!("{id3}"), "gamma");
    }

    #[test]
    fn plugin_id_serde_round_trip() {
        let id = PluginId::new("alpha");
        let s = serde_json::to_string(&id).expect("serialize");
        assert_eq!(s, "\"alpha\"");
        let back: PluginId = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn contribution_kind_serde_round_trip() {
        for kind in [
            ContributionKind::Skill,
            ContributionKind::Hook,
            ContributionKind::Mcp,
            ContributionKind::Connector,
            ContributionKind::SystemPrompt,
            ContributionKind::Command,
            ContributionKind::Agent,
        ] {
            let s = serde_json::to_string(&kind).expect("serialize");
            let back: ContributionKind = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(back, kind);
        }
    }

    /// Closed-enum invariant: every variant matched explicitly, no
    /// `_` wildcard. Adding a variant breaks this test (intentional).
    #[test]
    fn closed_enum_invariant() {
        fn label(k: ContributionKind) -> &'static str {
            match k {
                ContributionKind::Skill => "skill",
                ContributionKind::Hook => "hook",
                ContributionKind::Mcp => "mcp",
                ContributionKind::Connector => "connector",
                ContributionKind::SystemPrompt => "system_prompt",
                ContributionKind::Command => "command",
                ContributionKind::Agent => "agent",
            }
        }
        assert_eq!(label(ContributionKind::Skill), "skill");
        assert_eq!(label(ContributionKind::Hook), "hook");
        assert_eq!(label(ContributionKind::Mcp), "mcp");
        assert_eq!(label(ContributionKind::Connector), "connector");
        assert_eq!(label(ContributionKind::SystemPrompt), "system_prompt");
        assert_eq!(label(ContributionKind::Command), "command");
        assert_eq!(label(ContributionKind::Agent), "agent");
    }

    #[test]
    fn plugin_contributor_object_safe() {
        struct Dummy {
            id: PluginId,
        }
        impl PluginContributor for Dummy {
            fn plugin_id(&self) -> &PluginId {
                &self.id
            }
            fn contributions(&self) -> Vec<ContributionKind> {
                vec![ContributionKind::Skill]
            }
        }
        let d = Dummy {
            id: PluginId::new("dummy"),
        };
        let boxed: Box<dyn PluginContributor> = Box::new(d);
        assert_eq!(boxed.plugin_id().as_str(), "dummy");
        assert_eq!(boxed.contributions(), vec![ContributionKind::Skill]);
    }

    #[test]
    fn plugin_api_error_display() {
        let e = PluginApiError::Contributor("boom".into());
        assert_eq!(format!("{e}"), "contributor error: boom");
    }

    #[test]
    fn plugin_root_round_trip() {
        let root = PluginRoot {
            path: PathBuf::from("/tmp/x"),
        };
        let s = serde_json::to_string(&root).expect("serialize");
        let back: PluginRoot = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, root);
    }
}
