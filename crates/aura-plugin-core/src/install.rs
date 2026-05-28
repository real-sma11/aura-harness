//! Install pipeline: copy a source plugin tree into the cache, write
//! a normalised manifest, and atomically promote the active version.
//!
//! ## Steps (see [`install_with_trust`] for the canonical entry)
//!
//! 1. Verify `source` is a directory.
//! 2. Discover a manifest under `source` via [`crate::discover`].
//! 3. Re-validate the parsed manifest (defence in depth — the parser
//!    already validates, but a future caller could synthesise a
//!    manifest in memory and bypass parsing).
//! 4. If [`crate::TrustSection::require_explicit_trust`] is true and
//!    `trust_override = false`, fail with [`PluginInstallError::TrustRequired`].
//! 5. Compute the target dir = `cache.version_dir(id, version)`.
//! 6. If the target already exists AND it is the active version,
//!    return [`PluginInstallError::AlreadyInstalled`]. Callers that
//!    want to force-reinstall must remove the version dir first.
//! 7. **Atomic copy**:
//!    - Copy source tree into `<cache>/<id>/<version>.tmp/`.
//!    - Write the normalised manifest as
//!      `<target>/.aura-plugin.toml`.
//!    - `fs::rename` the `.tmp` dir into `<target>`.
//! 8. Promote via [`crate::PluginCache::set_active`].
//! 9. Return [`InstalledPlugin`] describing the result.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::cache::PluginCache;
use crate::discover::{discover_manifest, ManifestAlias};
use crate::error::PluginInstallError;
use crate::manifest::PluginManifest;

/// Outcome of a successful install.
#[derive(Clone, Debug)]
pub struct InstalledPlugin {
    /// Parsed + normalised manifest as installed.
    pub manifest: PluginManifest,
    /// Cache version dir containing the materialised payload.
    pub version_dir: PathBuf,
    /// Which alias the source used. The cache always writes back as
    /// `.aura-plugin.toml`; this field surfaces the alias purely for
    /// diagnostics / CLI messaging.
    pub source_alias: ManifestAlias,
}

/// Convenience wrapper around [`install_with_trust`] for the common
/// case (no trust override).
///
/// # Errors
///
/// See [`install_with_trust`].
pub fn install(
    source: impl AsRef<Path>,
    cache: &PluginCache,
) -> Result<InstalledPlugin, PluginInstallError> {
    install_with_trust(source, cache, false)
}

/// Install a plugin from `source` into `cache`. When `trust_override`
/// is true, the install pipeline ignores
/// [`crate::TrustSection::require_explicit_trust`]. The CLI passes
/// `true` for `--trust`; library callers default to `false`.
///
/// # Errors
///
/// See [`PluginInstallError`] for the full list.
pub fn install_with_trust(
    source: impl AsRef<Path>,
    cache: &PluginCache,
    trust_override: bool,
) -> Result<InstalledPlugin, PluginInstallError> {
    let source = source.as_ref();
    if !source.is_dir() {
        return Err(PluginInstallError::SourceNotDirectory(source.to_path_buf()));
    }

    let discovered = discover_manifest(source)?
        .ok_or_else(|| PluginInstallError::MissingManifest(source.to_path_buf()))?;
    discovered.manifest.validate()?;

    if discovered.manifest.trust.require_explicit_trust && !trust_override {
        return Err(PluginInstallError::TrustRequired(
            discovered.manifest.id.as_str().to_string(),
        ));
    }

    let id = discovered.manifest.id.as_str().to_string();
    let version = discovered.manifest.version.to_string();
    let target = cache.version_dir(&id, &version);
    if target.exists() {
        let active = cache.active_version(&id)?;
        if active.as_deref() == Some(version.as_str()) {
            return Err(PluginInstallError::AlreadyInstalled {
                id,
                existing: version,
            });
        }
    }

    let plugin_dir = cache.plugin_dir(&id);
    fs::create_dir_all(&plugin_dir)?;

    let tmp = plugin_dir.join(format!("{version}.tmp"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;

    copy_tree(source, &tmp)?;

    let normalised = discovered.manifest.to_toml_string()?;
    fs::write(tmp.join(".aura-plugin.toml"), normalised)?;

    if target.exists() {
        // Existing same-version dir that is not the active version
        // (e.g. a stale prior install); remove before rename so the
        // atomic promote can proceed.
        fs::remove_dir_all(&target)?;
    }

    match fs::rename(&tmp, &target) {
        Ok(()) => {}
        Err(_) => {
            // Cross-device or Windows-locked fallback: copy + remove.
            // Still safer than leaving a half-state — the version
            // dir is only observed via list_versions after this
            // function returns.
            copy_tree(&tmp, &target)?;
            fs::remove_dir_all(&tmp)?;
        }
    }

    cache.set_active(&id, &version)?;

    Ok(InstalledPlugin {
        manifest: discovered.manifest,
        version_dir: target,
        source_alias: discovered.alias,
    })
}

/// Recursively copy a directory tree, mirroring directory structure.
/// Symlinks are followed (Phase 4b consumers ship plain files; the
/// behaviour can be tightened in a later phase if needed).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).follow_links(true) {
        let entry = entry?;
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(src) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(root: &Path, id: &str, version: &str, require_trust: bool) {
        let manifest_dir = root.join(".aura-plugin");
        fs::create_dir_all(&manifest_dir).unwrap();
        let body = format!(
            r#"
manifest_version = "v1"
id = "{id}"
version = "{version}"
name = "{id}"
description = "fixture"

[trust]
require_explicit_trust = {require_trust}
"#
        );
        fs::write(manifest_dir.join("manifest.toml"), body).unwrap();
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(root.join("skills").join("intro.md"), "hello").unwrap();
    }

    #[test]
    fn install_populates_cache() {
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("my-plugin");
        fs::create_dir_all(&src).unwrap();
        write_fixture(&src, "my-plugin", "0.1.0", false);

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));

        let result = install(&src, &cache).expect("install");
        assert_eq!(result.manifest.id.as_str(), "my-plugin");
        assert_eq!(result.manifest.version.to_string(), "0.1.0");
        assert_eq!(result.source_alias, ManifestAlias::Aura);
        assert!(cache.version_dir("my-plugin", "0.1.0").exists());
        assert!(cache
            .version_dir("my-plugin", "0.1.0")
            .join(".aura-plugin.toml")
            .exists());
        assert!(cache
            .version_dir("my-plugin", "0.1.0")
            .join("skills")
            .join("intro.md")
            .exists());
        assert_eq!(
            cache.active_version("my-plugin").unwrap(),
            Some("0.1.0".to_string())
        );
    }

    #[test]
    fn install_rejects_missing_source() {
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        let err = install("c:/definitely/not/a/path/xyz", &cache).unwrap_err();
        assert!(matches!(err, PluginInstallError::SourceNotDirectory(_)));
    }

    #[test]
    fn install_rejects_missing_manifest() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        let err = install(src.path(), &cache).unwrap_err();
        assert!(matches!(err, PluginInstallError::MissingManifest(_)));
    }

    #[test]
    fn install_requires_trust_for_gated_manifest() {
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("gated");
        fs::create_dir_all(&src).unwrap();
        write_fixture(&src, "gated", "0.1.0", true);
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        let err = install(&src, &cache).unwrap_err();
        assert!(matches!(err, PluginInstallError::TrustRequired(_)));
    }

    #[test]
    fn install_with_trust_bypasses_gate() {
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("gated");
        fs::create_dir_all(&src).unwrap();
        write_fixture(&src, "gated", "0.1.0", true);
        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        let r = install_with_trust(&src, &cache, true).expect("install with trust");
        assert_eq!(r.manifest.id.as_str(), "gated");
    }

    #[test]
    fn install_normalises_codex_alias_to_aura_plugin_toml() {
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("codex-plug");
        fs::create_dir_all(src.join(".codex-plugin")).unwrap();
        fs::write(
            src.join(".codex-plugin").join("manifest.toml"),
            r#"
manifest_version = "v1"
id = "codex-plug"
version = "0.2.0"
"#,
        )
        .unwrap();

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        let r = install(&src, &cache).expect("install");
        assert_eq!(r.source_alias, ManifestAlias::Codex);
        let target_manifest = cache
            .version_dir("codex-plug", "0.2.0")
            .join(".aura-plugin.toml");
        assert!(
            target_manifest.exists(),
            "normalised manifest must live at .aura-plugin.toml even for codex alias"
        );
    }

    #[test]
    fn reinstall_same_active_version_errors() {
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("again");
        fs::create_dir_all(&src).unwrap();
        write_fixture(&src, "again", "0.1.0", false);

        let cache_dir = TempDir::new().unwrap();
        let cache = PluginCache::new(cache_dir.path().join("plugins"));
        install(&src, &cache).expect("first install");
        let err = install(&src, &cache).unwrap_err();
        assert!(
            matches!(err, PluginInstallError::AlreadyInstalled { .. }),
            "expected AlreadyInstalled, got {err:?}"
        );
    }
}
