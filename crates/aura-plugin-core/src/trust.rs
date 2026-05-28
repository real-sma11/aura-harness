//! First-enable trust prompt for installed plugins.
//!
//! ## Why this lives in `aura-plugin-core` (and not in the CLI bin)
//!
//! The on-disk install pipeline ([`crate::install_with_trust`]) only
//! decides whether the plugin can be **materialised**. The
//! `aura plugins enable` flow has a separate trust gate: when the
//! cached manifest's [`crate::TrustSection::require_explicit_trust`]
//! is `true`, the operator must explicitly confirm trust on first
//! enable. Putting the prompt logic here (rather than directly in
//! `src/main.rs`) lets the flow be exercised by `cargo test` without
//! a real TTY — the [`TrustPrompter`] trait is the seam.
//!
//! ## Invariants ([rules.md §13])
//!
//! - The trust decision is **per-plugin**, not per-version. Once an
//!   operator trusts a plugin id, subsequent upgrades inherit that
//!   trust (Phase 8 will revisit if the marketplace flow needs a
//!   per-version re-prompt).
//! - The flow reads the **active** cached version's manifest via
//!   [`crate::PluginCache::active_version`] + the canonical
//!   `.aura-plugin.toml` path. Manifests in non-active version dirs
//!   are not consulted.
//! - The CLI never auto-trusts a plugin behind the operator's back.
//!   The `--yes` flag is an explicit operator opt-in; the `--no`
//!   flag is an explicit operator opt-out. The default is a TTY
//!   prompt.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::cache::PluginCache;
use crate::manifest::PluginManifest;

/// Operator confirmation seam used by the enable-flow.
///
/// Real production builds wire a [`TtyPrompter`]; tests use
/// [`AlwaysYes`] / [`AlwaysNo`] so the enable flow can be exercised
/// without a real TTY.
pub trait TrustPrompter {
    /// Returns `true` iff the operator confirmed trust.
    ///
    /// `summary` is a human-readable one-line summary of the
    /// plugin's contributions (e.g. `"skills(2), hooks(0), mcp(1)"`),
    /// surfaced to the operator so the trust decision is informed.
    fn confirm(
        &mut self,
        plugin_id: &str,
        version: &str,
        source: Option<&str>,
        summary: &str,
    ) -> bool;
}

/// Always-yes prompter. Used for the `--yes` CLI flag and for unit
/// tests that exercise the "operator approved" branch.
pub struct AlwaysYes;

impl TrustPrompter for AlwaysYes {
    fn confirm(&mut self, _: &str, _: &str, _: Option<&str>, _: &str) -> bool {
        true
    }
}

/// Always-no prompter. Used for the `--no` CLI flag and for unit
/// tests that exercise the "operator declined" branch.
pub struct AlwaysNo;

impl TrustPrompter for AlwaysNo {
    fn confirm(&mut self, _: &str, _: &str, _: Option<&str>, _: &str) -> bool {
        false
    }
}

/// Interactive TTY prompter. Reads a single line from stdin and
/// treats `y` / `yes` (case-insensitive) as trust; anything else is
/// a decline.
pub struct TtyPrompter;

impl TrustPrompter for TtyPrompter {
    fn confirm(
        &mut self,
        plugin_id: &str,
        version: &str,
        source: Option<&str>,
        summary: &str,
    ) -> bool {
        eprintln!("Plugin `{plugin_id}` v{version} requests trust.");
        eprintln!("Source: {}", source.unwrap_or("unknown"));
        eprintln!("Contributes: {summary}");
        eprint!("Trust this plugin? [y/N]: ");
        if io::stderr().flush().is_err() {
            return false;
        }
        let mut line = String::new();
        let stdin = io::stdin();
        if stdin.lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Outcome of [`enable_with_prompter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableDecision {
    /// Plugin was enabled (and trusted, if a trust prompt fired).
    Enabled,
    /// Operator declined the trust prompt — plugin is NOT enabled.
    TrustDeclined,
    /// Plugin was already enabled + trusted from a prior run; the
    /// flow is a no-op.
    AlreadyTrusted,
}

/// Reasons [`enable_with_prompter`] can fail.
#[derive(Debug, thiserror::Error)]
pub enum EnableError {
    /// I/O error reading the cache or manifest.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// No active version is set for the plugin — install or
    /// activate it first.
    #[error("no active version for plugin `{0}`; install it first")]
    NotInstalled(String),
    /// Manifest parse failure for the cached `.aura-plugin.toml`.
    #[error(transparent)]
    Manifest(#[from] crate::error::ManifestError),
}

/// State the enable-flow needs to read from operator config (the
/// `[plugins.<id>]` table inside `AURA_HOME/config.toml`) to decide
/// whether to re-prompt.
///
/// The flow takes this snapshot as input rather than reading
/// `config.toml` itself so the I/O / TOML handling stays in the CLI
/// crate (where `toml` is already a dep) without dragging the
/// dependency into `aura-plugin-core`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PluginEnableState {
    /// `[plugins.<id>].enabled` from the prior config.toml. `None`
    /// when the section is missing entirely.
    pub enabled: Option<bool>,
    /// `[plugins.<id>].trusted` from the prior config.toml. `None`
    /// when the section is missing entirely.
    pub trusted: Option<bool>,
}

/// Side-effect-free decision produced by [`enable_with_prompter`].
///
/// The caller (the CLI handler in `src/main.rs`) translates this
/// into a `config.toml` write. Phase 4c does not write `config.toml`
/// inside the library so the I/O policy stays in the bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnableOutcome {
    /// What happened.
    pub decision: EnableDecision,
    /// Manifest version that was inspected. Useful for logging.
    pub version: String,
    /// Whether the plugin should now be marked `trusted = true` in
    /// operator config. Only `Some(true)` when the prompt confirmed
    /// or when the manifest did not require explicit trust at all.
    pub trusted_after: bool,
    /// Whether the plugin should now be marked `enabled = true` in
    /// operator config.
    pub enabled_after: bool,
}

/// Compute the enable decision for `id`, prompting via `prompter`
/// when the manifest requires explicit trust and the operator hasn't
/// already trusted it.
///
/// # Errors
///
/// See [`EnableError`].
pub fn enable_with_prompter<P: TrustPrompter>(
    cache: &PluginCache,
    id: &str,
    prior: PluginEnableState,
    prompter: &mut P,
) -> Result<EnableOutcome, EnableError> {
    let Some(version) = cache.active_version(id)? else {
        return Err(EnableError::NotInstalled(id.to_string()));
    };
    let version_dir = cache.version_dir(id, &version);
    let manifest = read_active_manifest(&version_dir)?;

    let require_trust = manifest.trust.require_explicit_trust;
    let already_trusted = prior.trusted == Some(true);

    if !require_trust {
        return Ok(EnableOutcome {
            decision: EnableDecision::Enabled,
            version,
            trusted_after: true,
            enabled_after: true,
        });
    }

    if already_trusted {
        return Ok(EnableOutcome {
            decision: EnableDecision::AlreadyTrusted,
            version,
            trusted_after: true,
            enabled_after: true,
        });
    }

    let summary = summarise_contributions(&manifest);
    let confirmed = prompter.confirm(id, &version, manifest.trust.source.as_deref(), &summary);
    if confirmed {
        Ok(EnableOutcome {
            decision: EnableDecision::Enabled,
            version,
            trusted_after: true,
            enabled_after: true,
        })
    } else {
        Ok(EnableOutcome {
            decision: EnableDecision::TrustDeclined,
            version,
            trusted_after: false,
            enabled_after: false,
        })
    }
}

fn read_active_manifest(version_dir: &Path) -> Result<PluginManifest, EnableError> {
    let path = version_dir.join(".aura-plugin.toml");
    let body = fs::read_to_string(&path)?;
    Ok(PluginManifest::from_toml_str(&body)?)
}

/// One-line human-readable summary of contribution counts. Surfaced
/// in the trust prompt so the operator can see what they're trusting
/// at a glance.
#[must_use]
pub fn summarise_contributions(m: &PluginManifest) -> String {
    let c = &m.contributes;
    format!(
        "skills({}), hooks({}), mcp({}), connectors({}), commands({}), agents({}), system_prompts({})",
        c.skills.len(),
        c.hooks.len(),
        c.mcp.len(),
        c.connectors.len(),
        c.commands.len(),
        c.agents.len(),
        c.system_prompts.len(),
    )
}

impl fmt::Display for EnableDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Enabled => "enabled",
            Self::TrustDeclined => "trust declined; plugin not enabled",
            Self::AlreadyTrusted => "already trusted; plugin enabled",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_manifest(version_dir: &Path, body: &str) {
        fs::create_dir_all(version_dir).unwrap();
        fs::write(version_dir.join(".aura-plugin.toml"), body).unwrap();
    }

    fn make_cache_with_active(id: &str, version: &str, body: &str) -> (TempDir, PluginCache) {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        let dir = cache.version_dir(id, version);
        write_manifest(&dir, body);
        cache.set_active(id, version).unwrap();
        (tmp, cache)
    }

    fn manifest_body(id: &str, version: &str, require_trust: bool) -> String {
        format!(
            r#"
manifest_version = "v1"
id = "{id}"
version = "{version}"

[trust]
source = "first-party"
require_explicit_trust = {require_trust}
"#
        )
    }

    #[test]
    fn always_yes_returns_true() {
        let mut p = AlwaysYes;
        assert!(p.confirm("id", "0.1.0", Some("first-party"), "summary"));
    }

    #[test]
    fn always_no_returns_false() {
        let mut p = AlwaysNo;
        assert!(!p.confirm("id", "0.1.0", Some("first-party"), "summary"));
    }

    #[test]
    fn no_explicit_trust_skips_prompt_and_enables() {
        let (_tmp, cache) =
            make_cache_with_active("open", "0.1.0", &manifest_body("open", "0.1.0", false));
        let mut p = AlwaysNo; // never gets called when require_trust is false
        let out = enable_with_prompter(&cache, "open", PluginEnableState::default(), &mut p)
            .expect("enable ok");
        assert_eq!(out.decision, EnableDecision::Enabled);
        assert!(out.enabled_after);
        assert!(out.trusted_after);
    }

    #[test]
    fn explicit_trust_required_and_operator_approves() {
        let (_tmp, cache) =
            make_cache_with_active("gated", "0.1.0", &manifest_body("gated", "0.1.0", true));
        let mut p = AlwaysYes;
        let out = enable_with_prompter(&cache, "gated", PluginEnableState::default(), &mut p)
            .expect("enable ok");
        assert_eq!(out.decision, EnableDecision::Enabled);
        assert!(out.enabled_after);
        assert!(out.trusted_after);
    }

    #[test]
    fn explicit_trust_required_and_operator_declines() {
        let (_tmp, cache) =
            make_cache_with_active("gated", "0.1.0", &manifest_body("gated", "0.1.0", true));
        let mut p = AlwaysNo;
        let out = enable_with_prompter(&cache, "gated", PluginEnableState::default(), &mut p)
            .expect("enable ok");
        assert_eq!(out.decision, EnableDecision::TrustDeclined);
        assert!(!out.enabled_after);
        assert!(!out.trusted_after);
    }

    #[test]
    fn already_trusted_short_circuits() {
        let (_tmp, cache) =
            make_cache_with_active("gated", "0.1.0", &manifest_body("gated", "0.1.0", true));
        let mut p = AlwaysNo; // would decline; must NOT be called
        let prior = PluginEnableState {
            enabled: Some(true),
            trusted: Some(true),
        };
        let out = enable_with_prompter(&cache, "gated", prior, &mut p).expect("enable ok");
        assert_eq!(out.decision, EnableDecision::AlreadyTrusted);
        assert!(out.enabled_after);
        assert!(out.trusted_after);
    }

    #[test]
    fn missing_active_version_errors() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        let mut p = AlwaysYes;
        let err = enable_with_prompter(&cache, "ghost", PluginEnableState::default(), &mut p)
            .unwrap_err();
        assert!(matches!(err, EnableError::NotInstalled(id) if id == "ghost"));
    }

    #[test]
    fn contribution_summary_counts_each_section() {
        let body = r#"
manifest_version = "v1"
id = "sum"
version = "0.1.0"

[[contributes.skills]]
id = "s1"
path = "./s.md"

[[contributes.hooks]]
event = "session_start"
command = "./h.sh"

[[contributes.mcp]]
server_id = "echo"
command = "./mcp"

[[contributes.connectors]]
id = "c1"
endpoint = "https://x"
"#;
        let m = PluginManifest::from_toml_str(body).unwrap();
        let s = summarise_contributions(&m);
        assert!(s.contains("skills(1)"));
        assert!(s.contains("hooks(1)"));
        assert!(s.contains("mcp(1)"));
        assert!(s.contains("connectors(1)"));
        assert!(s.contains("commands(0)"));
    }
}
