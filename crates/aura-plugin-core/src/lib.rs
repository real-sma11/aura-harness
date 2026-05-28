//! # aura-plugin-core
//!
//! Layer: plugin
//!
//! Declarative plugin manifest parse + install + cache layout +
//! marketplace lookup. Phase 4b deliverable: the on-disk pipeline is
//! end-to-end but **inert** — no agent-loop wiring (that's Phase 4c
//! for hooks/MCP/connectors and Phase 8 for full integration).
//!
//! ## Surfaces
//!
//! - [`PluginManifest`] / [`PluginManifestVersion`] — canonical
//!   schema parsed from `.aura-plugin/manifest.toml`. Compat aliases
//!   ([`discover::ManifestAlias`]) cover `.codex-plugin/manifest.toml`
//!   and `.claude-plugin/manifest.toml` and normalise into the same
//!   [`PluginManifest`] shape.
//! - [`install`] / [`install_with_trust`] — copy a source plugin
//!   directory into the [`PluginCache`] layout, writing a normalised
//!   `.aura-plugin.toml` at the cache version dir root.
//! - [`PluginCache`] — `<plugin_id>/<version>/` layout with an
//!   `active` pointer (text file on Windows, symlink-or-text on Unix).
//! - [`marketplace::MarketplaceManifest`] — wire format for the
//!   future online registry. Phase 4b ships an offline-only parser +
//!   lookup; no network calls.
//! - Error types: [`ManifestError`], [`PluginInstallError`].
//!
//! ## Invariants ([rules.md §13])
//!
//! - **Atomic install**: the install pipeline copies into
//!   `<id>/<version>.tmp/` then `fs::rename`s into place. A partial
//!   copy never leaves a `<id>/<version>/` directory observable to
//!   `list_versions()`.
//! - **Active version**: `<id>/active` always points at a directory
//!   that exists in the cache. `set_active` writes via a `.tmp` +
//!   rename for the same atomicity guarantee.
//! - **Compat-alias normalisation**: a manifest discovered in
//!   `.codex-plugin/` or `.claude-plugin/` is parsed, normalised
//!   into [`PluginManifest`], and written back to the cache as
//!   `.aura-plugin.toml`. The original on-disk filename is recorded
//!   in [`install::InstalledPlugin::source_alias`] for diagnostics.
//! - **First-party identity**: `aura-plugin-api::PluginId` is the
//!   shared identity newtype. We re-export it to keep the
//!   plugin-pipeline call sites on a single type.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod cache;
pub mod discover;
pub mod error;
pub mod install;
pub mod manifest;
pub mod marketplace;

pub use aura_plugin_api::{ContributionKind, PluginContributor, PluginId, PluginRoot};

pub use cache::PluginCache;
pub use discover::{discover_manifest, DiscoveredManifest, ManifestAlias};
pub use error::{ManifestError, PluginInstallError};
pub use install::{install, install_with_trust, InstalledPlugin};
pub use manifest::{
    AgentContribution, CommandContribution, ConnectorContribution, ContributesSection,
    HookContribution, McpContribution, PluginManifest, PluginManifestVersion, SkillContribution,
    SystemPromptContribution, TrustSection,
};
pub use marketplace::{MarketplaceEntry, MarketplaceManifest};
