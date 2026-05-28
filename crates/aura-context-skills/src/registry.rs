//! In-memory registry of loaded skills with precedence-based deduplication.

use crate::error::SkillError;
use crate::loader::SkillLoader;
use crate::types::{Skill, SkillMeta};
use aura_core::{Registry, RegistryError};
use std::collections::HashMap;
use tracing::{debug, warn};

/// In-memory registry mapping skill names to resolved [`Skill`] instances.
///
/// When multiple sources provide a skill with the same name, the one with the
/// highest [`SkillSource::precedence`](crate::types::SkillSource::precedence)
/// wins.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reload the registry from the given loader, replacing all entries.
    pub fn reload(&mut self, loader: &SkillLoader) {
        self.skills.clear();

        for result in loader.load_all() {
            match result {
                Ok(skill) => {
                    let name = skill.frontmatter.name.clone();
                    let new_precedence = skill.source.precedence();

                    if let Some(existing) = self.skills.get(&name) {
                        if new_precedence <= existing.source.precedence() {
                            debug!(
                                "skipping {name} from {} (existing from {} has equal or higher precedence)",
                                skill.source, existing.source
                            );
                            continue;
                        }
                        debug!("overriding {name}: {} -> {}", existing.source, skill.source);
                    }

                    self.skills.insert(name, skill);
                }
                Err(e) => {
                    warn!("failed to load skill: {e}");
                }
            }
        }
    }

    /// Get a skill by name.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::NotFound`] if no skill with the given name is registered.
    pub fn get(&self, name: &str) -> Result<&Skill, SkillError> {
        self.skills
            .get(name)
            .ok_or_else(|| SkillError::NotFound(name.to_string()))
    }

    /// Return metadata for all skills where model invocation is **not** disabled.
    #[must_use]
    pub fn model_invocable_metadata(&self) -> Vec<SkillMeta> {
        self.skills
            .values()
            .filter(|s| !s.frontmatter.disable_model_invocation.unwrap_or(false))
            .map(skill_to_meta)
            .collect()
    }

    /// Return metadata for all user-invocable skills.
    #[must_use]
    pub fn user_invocable_metadata(&self) -> Vec<SkillMeta> {
        self.skills
            .values()
            .filter(|s| s.frontmatter.user_invocable.unwrap_or(false))
            .map(skill_to_meta)
            .collect()
    }

    /// Return metadata for all registered skills.
    #[must_use]
    pub fn all_metadata(&self) -> Vec<SkillMeta> {
        self.skills.values().map(skill_to_meta).collect()
    }

    /// Return skills whose `paths` globs match any of the given file paths.
    ///
    /// This is a simple prefix/contains check — full glob matching can be added
    /// later.
    #[must_use]
    pub fn skills_for_paths(&self, paths: &[String]) -> Vec<&Skill> {
        self.skills
            .values()
            .filter(|s| {
                s.frontmatter.paths.as_ref().is_some_and(|skill_paths| {
                    skill_paths.iter().any(|pattern| {
                        paths
                            .iter()
                            .any(|p| p.contains(pattern) || pattern.contains(p))
                    })
                })
            })
            .collect()
    }

    /// Return references to all registered skills.
    #[must_use]
    pub fn all_skills(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    /// Number of skills in the registry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Add plugin-contributed skill roots to the registry.
    ///
    /// Each `root` is treated like the existing `extra_dirs`
    /// loader entry: every direct sub-directory of `root` that
    /// contains a `SKILL.md` file is parsed and registered with
    /// [`SkillSource::Extra`](crate::types::SkillSource::Extra).
    ///
    /// Per the Phase 8 invariant: plugin skill roots are merged
    /// AFTER personal / agent / workspace roots; a workspace skill
    /// with the same name as a plugin skill keeps the slot via
    /// the existing precedence rules.
    ///
    /// Empty `roots` is a no-op, preserving the empty-install
    /// backward-compat invariant.
    pub fn add_plugin_roots(&mut self, roots: &[std::path::PathBuf]) {
        if roots.is_empty() {
            return;
        }
        let cfg = crate::loader::SkillLoaderConfig {
            extra_dirs: roots.to_vec(),
            ..crate::loader::SkillLoaderConfig::default()
        };
        let loader = crate::loader::SkillLoader::new(cfg);
        for result in loader.load_all() {
            match result {
                Ok(skill) => {
                    let name = skill.frontmatter.name.clone();
                    let new_precedence = skill.source.precedence();
                    if let Some(existing) = self.skills.get(&name) {
                        if new_precedence <= existing.source.precedence() {
                            tracing::debug!(
                                "plugin skill {name} ignored (existing source has equal/higher precedence)"
                            );
                            continue;
                        }
                    }
                    self.skills.insert(name, skill);
                }
                Err(err) => {
                    tracing::warn!("failed to load plugin skill: {err}");
                }
            }
        }
    }
}

/// `Registry` trait impl (Wave 4 unification). The concrete inherent
/// methods above are retained for ergonomic / borrow-based access; this
/// impl exposes a single clone-based `Id -> Item` abstraction so call
/// sites can work across skill, tool, and automaton registries
/// generically.
impl Registry for SkillRegistry {
    type Id = String;
    type Item = Skill;

    fn register(&mut self, id: Self::Id, item: Self::Item) -> Result<(), RegistryError> {
        if self.skills.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.skills.insert(id, item);
        Ok(())
    }

    fn get(&self, id: &Self::Id) -> Option<Self::Item> {
        self.skills.get(id).cloned()
    }

    fn iter(&self) -> Vec<(Self::Id, Self::Item)> {
        self.skills
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn remove(&mut self, id: &Self::Id) -> Option<Self::Item> {
        self.skills.remove(id)
    }

    fn len(&self) -> usize {
        self.skills.len()
    }

    fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Convert a [`Skill`] to lightweight [`SkillMeta`].
pub fn skill_to_meta(skill: &Skill) -> SkillMeta {
    SkillMeta {
        name: skill.frontmatter.name.clone(),
        description: skill.frontmatter.description.clone(),
        source: skill.source.clone(),
        model_invocable: !skill.frontmatter.disable_model_invocation.unwrap_or(false),
        user_invocable: skill.frontmatter.user_invocable.unwrap_or(false),
        requested_paths: skill.frontmatter.allowed_paths.clone().unwrap_or_default(),
        requested_commands: skill
            .frontmatter
            .allowed_commands
            .clone()
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::SkillLoaderConfig;
    use tempfile::TempDir;

    fn make_skill_dir(base: &std::path::Path, name: &str, desc: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nBody for {name}."),
        )
        .unwrap();
    }

    #[test]
    fn precedence_override() {
        let tmp = TempDir::new().unwrap();
        let workspace_skills = tmp.path().join("ws").join("skills");
        let personal_skills = tmp.path().join("personal");

        make_skill_dir(&workspace_skills, "deploy", "workspace version");
        make_skill_dir(&personal_skills, "deploy", "personal version");

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().join("ws")),
            personal_dir: Some(personal_skills),
            ..SkillLoaderConfig::default()
        });

        let mut reg = SkillRegistry::new();
        reg.reload(&loader);

        let skill = reg.get("deploy").unwrap();
        assert_eq!(skill.frontmatter.description, "workspace version");
    }

    #[test]
    fn get_not_found() {
        let reg = SkillRegistry::new();
        assert!(reg.get("nonexistent").is_err());
    }

    #[test]
    fn model_invocable_metadata_filters() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws").join("skills");

        let dir = ws.join("visible");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: visible\ndescription: shown\n---\nBody.",
        )
        .unwrap();

        let dir2 = ws.join("hidden");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("SKILL.md"),
            "---\nname: hidden\ndescription: not shown\ndisable-model-invocation: true\n---\nBody.",
        )
        .unwrap();

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().join("ws")),
            ..SkillLoaderConfig::default()
        });
        let mut reg = SkillRegistry::new();
        reg.reload(&loader);

        let meta = reg.model_invocable_metadata();
        assert!(meta.iter().any(|m| m.name == "visible"));
        assert!(!meta.iter().any(|m| m.name == "hidden"));
    }

    #[test]
    fn user_invocable_metadata_filters() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws").join("skills");

        let dir = ws.join("user-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: user-skill\ndescription: user can invoke\nuser-invocable: true\n---\nBody.",
        )
        .unwrap();

        let dir2 = ws.join("model-only");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("SKILL.md"),
            "---\nname: model-only\ndescription: not user invocable\n---\nBody.",
        )
        .unwrap();

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().join("ws")),
            ..SkillLoaderConfig::default()
        });
        let mut reg = SkillRegistry::new();
        reg.reload(&loader);

        let meta = reg.user_invocable_metadata();
        assert!(meta.iter().any(|m| m.name == "user-skill"));
        assert!(!meta.iter().any(|m| m.name == "model-only"));
    }

    #[test]
    fn skills_for_paths_empty_paths() {
        let reg = SkillRegistry::new();
        let result = reg.skills_for_paths(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn registry_trait_basic_ops() {
        use crate::types::SkillSource;
        use aura_core::Registry;

        let mut reg = SkillRegistry::new();
        assert!(Registry::is_empty(&reg));
        assert_eq!(Registry::len(&reg), 0);

        let skill = Skill {
            frontmatter: crate::types::SkillFrontmatter {
                name: "demo".to_string(),
                description: "demo skill".to_string(),
                ..Default::default()
            },
            body: "body".to_string(),
            source: SkillSource::Workspace,
            dir_path: std::path::PathBuf::from("."),
        };

        Registry::register(&mut reg, "demo".to_string(), skill.clone())
            .expect("register should succeed");
        assert_eq!(Registry::len(&reg), 1);
        let got = Registry::get(&reg, &"demo".to_string()).expect("lookup by id");
        assert_eq!(got.frontmatter.name, "demo");

        let err = Registry::register(&mut reg, "demo".to_string(), skill)
            .expect_err("duplicate insert must error");
        assert!(matches!(err, aura_core::RegistryError::Duplicate(ref id) if id == "demo"));

        let ids: Vec<_> = Registry::iter(&reg).into_iter().map(|(k, _)| k).collect();
        assert_eq!(ids, vec!["demo".to_string()]);

        let removed = Registry::remove(&mut reg, &"demo".to_string()).expect("remove existing");
        assert_eq!(removed.frontmatter.name, "demo");
        assert!(Registry::is_empty(&reg));
    }

    #[test]
    fn skills_for_paths_no_match() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws").join("skills");
        let dir = ws.join("path-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: path-skill\ndescription: test\npaths:\n  - src/components\n---\nBody.",
        )
        .unwrap();

        let loader = SkillLoader::new(SkillLoaderConfig {
            workspace_root: Some(tmp.path().join("ws")),
            ..SkillLoaderConfig::default()
        });
        let mut reg = SkillRegistry::new();
        reg.reload(&loader);

        let result = reg.skills_for_paths(&["tests/unit".to_string()]);
        assert!(result.is_empty());
    }
}
