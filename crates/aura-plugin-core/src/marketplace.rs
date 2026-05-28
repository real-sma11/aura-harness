//! Marketplace registry lookup.
//!
//! Phase 4b ships an **offline-stub** registry — no network calls.
//! The [`MarketplaceManifest`] shape is the canonical wire format
//! for future online registries. Phase 4c+ may wire a real client;
//! Phase 4b verifies parse + lookup against fixtures.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Parsing is **pure** — no I/O is hidden inside [`from_toml_str`]
//!   or [`MarketplaceManifest::find`]. The caller owns reading
//!   `marketplace.toml` off disk and passing the string in.
//! - [`MarketplaceManifest::find`] does **exact** match on `id`.
//!   Future versions may add fuzzy / prefix matching behind a
//!   separate API; today's call sites should expect a single
//!   canonical id per query.

use serde::{Deserialize, Serialize};

/// Top-level marketplace manifest. Loaded by the CLI from
/// `<AURA_HOME>/plugins/marketplace.toml` once the online registry
/// lands; today the only consumer is the offline-fixture test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MarketplaceManifest {
    /// Identifier for this marketplace (used in `name@market` plugin
    /// identities once the richer identity lands; today purely
    /// informational).
    pub registry: String,
    /// Catalogue of plugins. Order is preserved.
    pub plugins: Vec<MarketplaceEntry>,
}

/// Single marketplace entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MarketplaceEntry {
    /// Plugin id (matches [`crate::PluginManifest::id`]).
    pub id: String,
    /// Latest known version (semver string). The install pipeline
    /// resolves this against the cache to decide whether to upgrade.
    pub latest_version: String,
    /// Source URL the registry would clone / download from. Phase 4b
    /// stores but does not consume this field.
    pub source_url: String,
    /// Optional one-line description surfaced by `aura plugins list`.
    #[serde(default)]
    pub description: Option<String>,
}

impl MarketplaceManifest {
    /// Parse a marketplace manifest from TOML.
    ///
    /// # Errors
    ///
    /// Returns [`toml::de::Error`] for TOML parse failures. Schema
    /// validation is intentionally light today — the wire format
    /// will be extended in Phase 4c+ when the online registry lands.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Exact-id lookup. `None` if the id is not in the catalogue.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|p| p.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
registry = "official"

[[plugins]]
id = "alpha"
latest_version = "1.0.0"
source_url = "https://example.com/alpha"
description = "Alpha plugin"

[[plugins]]
id = "beta"
latest_version = "0.2.0"
source_url = "https://example.com/beta"
"#;

    #[test]
    fn parses_marketplace_fixture() {
        let m = MarketplaceManifest::from_toml_str(FIXTURE).expect("parse");
        assert_eq!(m.registry, "official");
        assert_eq!(m.plugins.len(), 2);
        assert_eq!(m.plugins[0].id, "alpha");
        assert_eq!(m.plugins[0].latest_version, "1.0.0");
        assert_eq!(m.plugins[0].description.as_deref(), Some("Alpha plugin"));
        assert_eq!(m.plugins[1].description, None);
    }

    #[test]
    fn find_returns_entry_or_none() {
        let m = MarketplaceManifest::from_toml_str(FIXTURE).unwrap();
        assert_eq!(m.find("alpha").unwrap().latest_version, "1.0.0");
        assert!(m.find("nonexistent").is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let m = MarketplaceManifest::from_toml_str(FIXTURE).unwrap();
        let s = toml::to_string(&m).expect("serialise");
        let back = MarketplaceManifest::from_toml_str(&s).unwrap();
        assert_eq!(m, back);
    }
}
