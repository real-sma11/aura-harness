//! Plugin manifest schema.
//!
//! ## Compatibility matrix ([rules.md §13 invariants])
//!
//! - `.aura-plugin/manifest.toml` is the **canonical** schema for V1.
//! - `.codex-plugin/manifest.toml` and `.claude-plugin/manifest.toml`
//!   are **READ-ONLY compat aliases**: parsed, normalised into
//!   [`PluginManifest`], then written back to the cache as
//!   `.aura-plugin.toml`.
//! - Local active-version wins over semver: when two manifests share
//!   the same `id`, the one with `active = true` is preferred; ties
//!   go to the higher semver. (The discover-time tie-break helper
//!   lives in [`crate::discover`]; this module just defines the
//!   field that signals it.)
//!
//! ## Field shape
//!
//! Mirrors the Codex `RawPluginManifest` shape from the plan, scoped
//! down to what Phase 4b needs: identity, version, optional metadata,
//! and a [`ContributesSection`] of contribution lists. Each
//! contribution shape is intentionally minimal in V1 — the runtime
//! crates landing in Phase 4c flesh out the wire formats.

use std::collections::BTreeMap;

use semver::Version;
use serde::{Deserialize, Serialize};

use aura_plugin_api::PluginId;

use crate::error::ManifestError;

/// Schema version. Future-proof for breaking schema bumps; today
/// only [`PluginManifestVersion::V1`] is supported, and any other
/// integer parses to [`ManifestError::UnsupportedVersion`].
///
/// Wire format is the literal string `"v1"` (lowercase) to match the
/// `manifest_version = "v1"` syntax in the example fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginManifestVersion {
    /// Schema version 1.
    V1,
}

impl PluginManifestVersion {
    /// Numeric form used in [`ManifestError::UnsupportedVersion`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::V1 => 1,
        }
    }
}

/// Canonical parsed manifest.
///
/// Serialization quirks:
///
/// - `id` is a string (the [`PluginId`] newtype is serde-transparent).
/// - `version` is a semver string (`semver` crate handles the
///   round-trip via its `serde` feature).
/// - `meta` accepts an arbitrary TOML table for plugin-author
///   ergonomics; it round-trips as-is. Because `toml::Value` can
///   carry floats it is `PartialEq` but not `Eq`, so [`PluginManifest`]
///   intentionally derives only `PartialEq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Schema version (1 today).
    pub manifest_version: PluginManifestVersion,
    /// Plugin identity.
    pub id: PluginId,
    /// Semantic version (`semver::Version`).
    pub version: Version,
    /// Human-readable plugin name (separate from `id`).
    #[serde(default)]
    pub name: Option<String>,
    /// One-line description for `aura plugins list`.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether this manifest is the active version on disk. Used by
    /// [`crate::discover`] to break ties between multiple installed
    /// versions of the same plugin id (local `active = true` always
    /// wins over a higher semver).
    #[serde(default)]
    pub active: bool,
    /// Contributions advertised by this plugin.
    #[serde(default)]
    pub contributes: ContributesSection,
    /// Trust + provenance metadata.
    #[serde(default)]
    pub trust: TrustSection,
    /// Free-form `meta` table for plugin authors. Aura makes no
    /// guarantees about this section; runtime crates that want to
    /// expose plugin-author metadata should namespace under
    /// `meta.<crate>`.
    #[serde(default)]
    pub meta: BTreeMap<String, toml::Value>,
}

impl PluginManifest {
    /// Parse a TOML manifest from string form. Validates schema
    /// version and surfaces TOML / version errors through
    /// [`ManifestError`].
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Toml`] for parse failures and
    /// [`ManifestError::InvalidSchema`] for schema-validation
    /// failures (e.g. empty `id`).
    pub fn from_toml_str(s: &str) -> Result<Self, ManifestError> {
        let parsed: Self = toml::from_str(s)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Serialise back to TOML form for cache write-back.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::InvalidSchema`] only if the manifest
    /// is in an internally inconsistent state (validation runs
    /// first). TOML serialisation itself does not fail for the
    /// shapes used here.
    pub fn to_toml_string(&self) -> Result<String, ManifestError> {
        self.validate()?;
        toml::to_string_pretty(self)
            .map_err(|e| ManifestError::InvalidSchema(format!("toml serialize failed: {e}")))
    }

    /// Schema-level validation. Called by [`Self::from_toml_str`]
    /// after parse and by [`Self::to_toml_string`] before serialize.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::InvalidSchema`] when any required
    /// invariant is violated.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.id.as_str().trim().is_empty() {
            return Err(ManifestError::InvalidSchema(
                "manifest `id` must not be empty".to_string(),
            ));
        }
        for skill in &self.contributes.skills {
            if skill.id.trim().is_empty() {
                return Err(ManifestError::InvalidSchema(
                    "skill contribution `id` must not be empty".to_string(),
                ));
            }
            if skill.path.contains("..") {
                return Err(ManifestError::InvalidSchema(format!(
                    "skill path `{}` contains parent-dir traversal",
                    skill.path
                )));
            }
        }
        Ok(())
    }
}

/// Lists of contributions advertised by a plugin. Each variant is a
/// [`Vec`] (default empty) so a manifest can omit any section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ContributesSection {
    /// Skill (system-prompt-attached) contributions.
    pub skills: Vec<SkillContribution>,
    /// Lifecycle hook contributions.
    pub hooks: Vec<HookContribution>,
    /// MCP server contributions.
    pub mcp: Vec<McpContribution>,
    /// Connector contributions.
    pub connectors: Vec<ConnectorContribution>,
    /// Slash-command / CLI command contributions.
    pub commands: Vec<CommandContribution>,
    /// Subagent recipe contributions.
    pub agents: Vec<AgentContribution>,
    /// System-prompt addendum contributions.
    pub system_prompts: Vec<SystemPromptContribution>,
}

/// Trust + provenance metadata.
///
/// `require_explicit_trust = false` means the manifest can be
/// auto-trusted at install time (the default for first-party shipping
/// plugins). When `true`, the install pipeline refuses to materialise
/// the plugin without `install_with_trust(..., true)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct TrustSection {
    /// Plugin author / source identifier (e.g., `"first-party"`,
    /// `"marketplace:awesome-plugins"`, or a public-key fingerprint).
    pub source: Option<String>,
    /// Whether the operator must explicitly trust this manifest on
    /// first install / activation. Defaults to `false` so
    /// first-party shipping manifests install without a prompt.
    pub require_explicit_trust: bool,
}

/// Single skill contribution (system-prompt-attached doc).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SkillContribution {
    /// Stable skill identifier.
    pub id: String,
    /// Relative path under the plugin root to the skill content.
    pub path: String,
}

/// Single lifecycle hook contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HookContribution {
    /// Hook event name (see Phase 4c [`HookEvent`] enum once it
    /// lands). Today this is a free-form string; Phase 4c validates
    /// against the closed event enum.
    pub event: String,
    /// Command binary path (relative to plugin root for plugin
    /// scripts, absolute for system binaries).
    pub command: String,
    /// Command arguments. Default empty.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Single MCP server contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct McpContribution {
    /// Stable MCP server identifier (the merge key for
    /// first-active-wins resolution in Phase 4c).
    pub server_id: String,
    /// Command binary to launch the MCP server as a subprocess.
    pub command: String,
    /// Command arguments. Default empty.
    #[serde(default)]
    pub args: Vec<String>,
    /// Env overrides applied when launching the server. Default
    /// empty.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Single connector contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectorContribution {
    /// Stable connector identifier.
    pub id: String,
    /// Connector endpoint URL or identifier (format owned by the
    /// future `aura-plugin-connectors` crate).
    pub endpoint: String,
}

/// Single slash-command / CLI command contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CommandContribution {
    /// User-facing command name (e.g. `"summarise"`).
    pub name: String,
    /// Command binary or script path.
    pub command: String,
}

/// Single subagent recipe contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentContribution {
    /// Stable agent recipe identifier.
    pub id: String,
    /// Relative path under the plugin root to the agent config file.
    pub config_path: String,
}

/// Single system-prompt addendum contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SystemPromptContribution {
    /// Stable system-prompt addendum identifier.
    pub id: String,
    /// Relative path under the plugin root to the prompt content.
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
manifest_version = "v1"
id = "my-plugin"
version = "0.1.0"
name = "My Plugin"
description = "Fixture"
"#
    }

    #[test]
    fn manifest_version_serialises_lowercase() {
        let v = PluginManifestVersion::V1;
        // serde_json gives a portable view of the wire-format string
        // ("v1") without forcing the variant through a top-level
        // toml::to_string call (which requires a table).
        let s = serde_json::to_string(&v).expect("ser json");
        assert_eq!(s, "\"v1\"");
        let back: PluginManifestVersion = serde_json::from_str(&s).expect("de json");
        assert_eq!(back, v);
        assert_eq!(v.as_u8(), 1);
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = PluginManifest::from_toml_str(minimal_toml()).expect("parse ok");
        assert_eq!(m.id.as_str(), "my-plugin");
        assert_eq!(m.version.to_string(), "0.1.0");
        assert_eq!(m.name.as_deref(), Some("My Plugin"));
        assert!(!m.active);
        assert!(m.contributes.skills.is_empty());
        assert!(m.trust.source.is_none());
        assert!(!m.trust.require_explicit_trust);
    }

    #[test]
    fn rejects_empty_id() {
        let bad = r#"
manifest_version = "v1"
id = ""
version = "0.1.0"
"#;
        let err = PluginManifest::from_toml_str(bad).expect_err("must reject empty id");
        assert!(matches!(err, ManifestError::InvalidSchema(_)));
    }

    #[test]
    fn rejects_unsupported_version_value() {
        let bad = r#"
manifest_version = "v99"
id = "x"
version = "0.1.0"
"#;
        let err = PluginManifest::from_toml_str(bad).expect_err("must reject unknown version");
        assert!(matches!(err, ManifestError::Toml(_)));
    }

    #[test]
    fn rejects_skill_with_parent_traversal() {
        let bad = r#"
manifest_version = "v1"
id = "x"
version = "0.1.0"

[[contributes.skills]]
id = "s"
path = "../etc/passwd"
"#;
        let err = PluginManifest::from_toml_str(bad).expect_err("must reject traversal");
        assert!(matches!(err, ManifestError::InvalidSchema(_)));
    }

    #[test]
    fn round_trips_through_toml() {
        let original = PluginManifest::from_toml_str(minimal_toml()).expect("parse");
        let serialised = original.to_toml_string().expect("serialise");
        let back = PluginManifest::from_toml_str(&serialised).expect("reparse");
        assert_eq!(original, back);
    }

    #[test]
    fn parses_full_contributes_section() {
        let full = r#"
manifest_version = "v1"
id = "full"
version = "1.0.0"
active = true

[trust]
source = "first-party"
require_explicit_trust = false

[[contributes.skills]]
id = "s1"
path = "./skills/s1.md"

[[contributes.hooks]]
event = "PreToolUse"
command = "./hooks/pre.sh"
args = ["--verbose"]

[[contributes.mcp]]
server_id = "echo"
command = "./mcp/echo"
args = []

[contributes.mcp.env]
DEBUG = "1"

[[contributes.connectors]]
id = "c1"
endpoint = "https://example.com"

[[contributes.commands]]
name = "summarise"
command = "./cmd/summarise"

[[contributes.agents]]
id = "spec"
config_path = "./agents/spec.toml"

[[contributes.system_prompts]]
id = "intro"
path = "./prompts/intro.md"

[meta]
authors = ["alice"]
"#;
        let m = PluginManifest::from_toml_str(full).expect("parse full");
        assert!(m.active);
        assert_eq!(m.trust.source.as_deref(), Some("first-party"));
        assert_eq!(m.contributes.skills.len(), 1);
        assert_eq!(m.contributes.hooks.len(), 1);
        assert_eq!(m.contributes.mcp.len(), 1);
        assert_eq!(
            m.contributes.mcp[0].env.get("DEBUG"),
            Some(&"1".to_string())
        );
        assert_eq!(m.contributes.connectors.len(), 1);
        assert_eq!(m.contributes.commands.len(), 1);
        assert_eq!(m.contributes.agents.len(), 1);
        assert_eq!(m.contributes.system_prompts.len(), 1);
        assert!(m.meta.contains_key("authors"));
    }
}
