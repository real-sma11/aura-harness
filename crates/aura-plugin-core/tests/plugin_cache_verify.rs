//! Phase 4b integration test: install a fixture plugin, verify the
//! [`PluginCache`] layout, and confirm the compat-alias normalisation
//! path writes a `.aura-plugin.toml` regardless of which alias the
//! source used.
//!
//! ## Filename note (Windows UAC)
//!
//! Prior runs hit a Windows UAC heuristic: an executable whose name
//! starts with `install*.exe` triggered the elevation prompt. Cargo
//! names integration test binaries `<filename>.exe`, so this file is
//! deliberately named `plugin_cache_verify.rs` rather than
//! `install_test.rs` to keep `install` out of the binary name.

use std::fs;

use aura_plugin_core::{
    discover_manifest, install, MarketplaceManifest, PluginCache, PluginInstallError,
};
use tempfile::TempDir;

const MARKETPLACE_FIXTURE: &str = r#"
registry = "phase-4b-test"

[[plugins]]
id = "my-plugin"
latest_version = "0.1.0"
source_url = "local://fixtures/my-plugin"
description = "Fixture for Phase 4b cache test"
"#;

fn write_aura_manifest(plugin_root: &std::path::Path, body: &str) {
    fs::create_dir_all(plugin_root.join(".aura-plugin")).unwrap();
    fs::write(plugin_root.join(".aura-plugin").join("manifest.toml"), body).unwrap();
}

fn write_codex_manifest(plugin_root: &std::path::Path, body: &str) {
    fs::create_dir_all(plugin_root.join(".codex-plugin")).unwrap();
    fs::write(
        plugin_root.join(".codex-plugin").join("manifest.toml"),
        body,
    )
    .unwrap();
}

#[test]
fn install_fixture_populates_cache() {
    let src = TempDir::new().expect("temp source dir");
    let plugin_root = src.path().join("my-plugin");
    fs::create_dir_all(&plugin_root).unwrap();
    write_aura_manifest(
        &plugin_root,
        r#"
manifest_version = "v1"
id = "my-plugin"
version = "0.1.0"
name = "My Plugin"
description = "Fixture for Phase 4b install test"
"#,
    );

    let cache_root = TempDir::new().expect("temp cache root");
    let cache = PluginCache::new(cache_root.path().join("plugins"));

    let installed = install(&plugin_root, &cache).expect("install must succeed");
    assert_eq!(installed.manifest.id.as_str(), "my-plugin");
    assert_eq!(installed.manifest.version.to_string(), "0.1.0");

    let version_dir = cache.version_dir("my-plugin", "0.1.0");
    assert!(
        version_dir.is_dir(),
        "version dir missing at {version_dir:?}"
    );
    assert!(
        version_dir.join(".aura-plugin.toml").is_file(),
        "normalised manifest missing"
    );
    assert_eq!(
        cache.active_version("my-plugin").unwrap(),
        Some("0.1.0".to_string()),
        "active pointer must record installed version"
    );

    assert_eq!(cache.list_plugins().unwrap(), vec!["my-plugin".to_string()]);
    assert_eq!(
        cache.list_versions("my-plugin").unwrap(),
        vec!["0.1.0".to_string()]
    );
}

#[test]
fn install_codex_compat_alias_normalises_to_aura_plugin_toml() {
    let src = TempDir::new().unwrap();
    let plugin_root = src.path().join("codex-aliased");
    fs::create_dir_all(&plugin_root).unwrap();
    write_codex_manifest(
        &plugin_root,
        r#"
manifest_version = "v1"
id = "codex-aliased"
version = "0.3.1"
description = "Lives under .codex-plugin, must normalise to .aura-plugin.toml"
"#,
    );

    let cache_root = TempDir::new().unwrap();
    let cache = PluginCache::new(cache_root.path().join("plugins"));

    let installed = install(&plugin_root, &cache).expect("install must succeed");
    assert_eq!(
        installed.source_alias,
        aura_plugin_core::ManifestAlias::Codex
    );

    let normalised = cache
        .version_dir("codex-aliased", "0.3.1")
        .join(".aura-plugin.toml");
    assert!(
        normalised.is_file(),
        "cache must always write normalised manifest at .aura-plugin.toml"
    );
}

#[test]
fn marketplace_fixture_lookup_works() {
    let m = MarketplaceManifest::from_toml_str(MARKETPLACE_FIXTURE).expect("parse marketplace");
    assert_eq!(m.registry, "phase-4b-test");
    let entry = m.find("my-plugin").expect("lookup must succeed");
    assert_eq!(entry.latest_version, "0.1.0");
    assert!(m.find("missing-plugin").is_none());
}

#[test]
fn discover_reports_missing_manifest_as_none() {
    let src = TempDir::new().unwrap();
    let plugin_root = src.path().join("empty");
    fs::create_dir_all(&plugin_root).unwrap();
    assert!(discover_manifest(&plugin_root)
        .expect("discover ok")
        .is_none());
}

#[test]
fn install_twice_yields_already_installed_error() {
    let src = TempDir::new().unwrap();
    let plugin_root = src.path().join("repeat");
    fs::create_dir_all(&plugin_root).unwrap();
    write_aura_manifest(
        &plugin_root,
        r#"
manifest_version = "v1"
id = "repeat"
version = "0.1.0"
"#,
    );

    let cache_root = TempDir::new().unwrap();
    let cache = PluginCache::new(cache_root.path().join("plugins"));
    install(&plugin_root, &cache).expect("first install ok");
    let err = install(&plugin_root, &cache).expect_err("second install must fail");
    assert!(
        matches!(err, PluginInstallError::AlreadyInstalled { .. }),
        "expected AlreadyInstalled, got {err:?}"
    );
}
