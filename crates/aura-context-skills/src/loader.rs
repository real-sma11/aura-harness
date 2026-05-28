//! Filesystem skill discovery and loading.
//!
//! Walks configured directories looking for sub-directories that contain a
//! `SKILL.md` file, parses each one, and returns a list of [`Skill`] values
//! with their source provenance attached.

use crate::error::SkillError;
use crate::parser::parse_skill_md;
use crate::types::{Skill, SkillSource};
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Maximum size of a `SKILL.md` file, in bytes.
///
/// A pathological or untrusted skill directory could otherwise load a
/// multi-gigabyte file directly into memory. 1 MiB is well above every
/// real-world skill we've shipped. (Wave 5 / T4.)
const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;

/// Configuration for the skill loader — specifies which directories to scan.
#[derive(Debug, Clone, Default)]
pub struct SkillLoaderConfig {
    /// Workspace root (used to locate `{workspace}/skills/`).
    pub workspace_root: Option<PathBuf>,
    /// Agent-specific personal skills directory (e.g. `~/.aura/agents/{id}/skills/`).
    pub agent_personal_dir: Option<PathBuf>,
    /// User-level personal skills directory (e.g. `~/.aura/skills/`).
    pub personal_dir: Option<PathBuf>,
    /// Bundled skills directory shipped with the runtime.
    pub bundled_dir: Option<PathBuf>,
    /// Additional directories from config.
    pub extra_dirs: Vec<PathBuf>,
}

/// Discovers and loads skills from the filesystem.
#[derive(Debug, Clone)]
pub struct SkillLoader {
    config: SkillLoaderConfig,
}

impl SkillLoader {
    /// Create a loader with the given directory configuration.
    #[must_use]
    pub const fn new(config: SkillLoaderConfig) -> Self {
        Self { config }
    }

    /// Create a loader using platform-default directories.
    ///
    /// Uses `dirs::home_dir()` to locate `~/.aura/skills/` (personal) and
    /// optionally `~/.aura/agents/{agent_id}/skills/` when an agent id is provided.
    #[must_use]
    pub fn with_defaults(workspace_root: Option<PathBuf>, agent_id: Option<&str>) -> Self {
        let home = dirs::home_dir();
        let aura_home = home.map(|h| h.join(".aura"));

        let personal_dir = aura_home.as_ref().map(|ah| ah.join("skills"));
        let agent_personal_dir = agent_id
            .zip(aura_home.as_ref())
            .map(|(id, ah)| ah.join("agents").join(id).join("skills"));

        Self::new(SkillLoaderConfig {
            workspace_root,
            agent_personal_dir,
            personal_dir,
            bundled_dir: None,
            extra_dirs: Vec::new(),
        })
    }

    /// Load all skills from all configured locations.
    ///
    /// Skills are returned in no particular order; callers should use the
    /// registry to apply precedence-based deduplication.
    #[must_use]
    pub fn load_all(&self) -> Vec<Result<Skill, SkillError>> {
        let mut results = Vec::new();

        if let Some(ref root) = self.config.workspace_root {
            let dir = root.join("skills");
            load_from_dir(&dir, &SkillSource::Workspace, &mut results);
        }

        if let Some(ref dir) = self.config.agent_personal_dir {
            load_from_dir(dir, &SkillSource::AgentPersonal, &mut results);
        }

        if let Some(ref dir) = self.config.personal_dir {
            load_from_dir(dir, &SkillSource::Personal, &mut results);
        }

        if let Some(ref dir) = self.config.bundled_dir {
            load_from_dir(dir, &SkillSource::Bundled, &mut results);
        }

        for dir in &self.config.extra_dirs {
            let source = SkillSource::Extra(dir.clone());
            load_from_dir(dir, &source, &mut results);
        }

        results
    }

    /// Get a reference to the config.
    #[must_use]
    pub const fn config(&self) -> &SkillLoaderConfig {
        &self.config
    }

    /// Get a mutable reference to the config for runtime adjustments.
    pub fn config_mut(&mut self) -> &mut SkillLoaderConfig {
        &mut self.config
    }
}

/// Scan `base_dir` for sub-directories containing `SKILL.md`, parse each one.
fn load_from_dir(base_dir: &Path, source: &SkillSource, out: &mut Vec<Result<Skill, SkillError>>) {
    let entries = match std::fs::read_dir(base_dir) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("cannot read skill directory {}: {e}", base_dir.display());
            }
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("error reading directory entry: {e}");
                continue;
            }
        };

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        debug!("loading skill from {}", skill_md.display());
        let result = load_single_skill(&skill_md, source.clone(), &path);
        out.push(result);
    }
}

/// Parse a single SKILL.md file into a [`Skill`].
///
/// Reads at most [`MAX_SKILL_MD_BYTES`] using a bounded `File::take` so a
/// hostile skill directory cannot OOM the loader. Files whose reported
/// metadata size already exceeds the cap are rejected up-front with
/// [`SkillError::TooLarge`]; shorter files are read into a `String` via
/// the same bounded reader. (Wave 5 / T4.)
fn load_single_skill(
    skill_md: &Path,
    source: SkillSource,
    dir_path: &Path,
) -> Result<Skill, SkillError> {
    let mut file = std::fs::File::open(skill_md)?;
    let meta = file.metadata()?;
    if meta.len() > MAX_SKILL_MD_BYTES {
        return Err(SkillError::TooLarge {
            path: skill_md.to_path_buf(),
            actual: meta.len(),
            limit: MAX_SKILL_MD_BYTES,
        });
    }
    let mut content = String::new();
    (&mut file)
        .take(MAX_SKILL_MD_BYTES)
        .read_to_string(&mut content)?;
    let (frontmatter, body) = parse_skill_md(&content)?;

    Ok(Skill {
        frontmatter,
        body,
        source,
        dir_path: dir_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_skills_from_directory() {
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("skills");
        let deploy_dir = skills_dir.join("deploy");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        std::fs::write(
            deploy_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: Deploy app\n---\nDeploy instructions here.",
        )
        .unwrap();

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().to_path_buf()),
            ..SkillLoaderConfig::default()
        });

        let skills: Vec<_> = loader
            .load_all()
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].frontmatter.name, "deploy");
        assert_eq!(skills[0].source, SkillSource::Workspace);
    }

    #[test]
    fn missing_directory_produces_no_error() {
        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(PathBuf::from("/nonexistent/path/xyz")),
            ..SkillLoaderConfig::default()
        });
        let results = loader.load_all();
        assert!(results.is_empty());
    }

    #[test]
    fn oversize_skill_md_is_rejected() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join("skills");
        let big_dir = skills_dir.join("bloated");
        std::fs::create_dir_all(&big_dir).unwrap();

        // Write just over the 1 MiB cap. Content doesn't have to parse:
        // the size check triggers before `parse_skill_md` runs.
        let path = big_dir.join("SKILL.md");
        let mut f = std::fs::File::create(&path).unwrap();
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..17 {
            f.write_all(&chunk).unwrap(); // ~1.06 MiB total
        }
        drop(f);

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().to_path_buf()),
            ..SkillLoaderConfig::default()
        });

        let results = loader.load_all();
        assert_eq!(results.len(), 1);
        match &results[0] {
            Err(SkillError::TooLarge { actual, limit, .. }) => {
                assert!(*actual > *limit);
                assert_eq!(*limit, super::MAX_SKILL_MD_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }
}
