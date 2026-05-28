//! Core types for the skill system.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Parsed YAML frontmatter from a `SKILL.md` file.
///
/// All fields are optional except `name` and `description` (which default to
/// empty strings). The field names follow the Claude Code / `AgentSkills` open
/// standard so that existing SKILL.md files are fully compatible.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkillFrontmatter {
    /// Human-readable skill name (lowercase, hyphens, digits, 1-64 chars).
    pub name: String,
    /// One-line description shown in the skill catalogue.
    pub description: String,
    /// SPDX license identifier for the skill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Compatibility string (e.g. agent runtime versions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// Arbitrary metadata blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Tool allowlist — only these tools may be used when the skill is active.
    #[serde(rename = "allowed-tools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// When `true`, the skill cannot invoke a sub-model.
    #[serde(
        rename = "disable-model-invocation",
        skip_serializing_if = "Option::is_none"
    )]
    pub disable_model_invocation: Option<bool>,
    /// When `true`, the skill can be triggered directly by the user.
    #[serde(rename = "user-invocable", skip_serializing_if = "Option::is_none")]
    pub user_invocable: Option<bool>,
    /// Context mode hint (e.g. "fork", "append").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Agent/sub-agent type to use when running this skill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Glob patterns for files the skill applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Preferred model for this skill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort / reasoning-effort hint (e.g. "low", "high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Hint shown to the user when invoking the skill.
    #[serde(rename = "argument-hint", skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Shell interpreter to use for backtick command injection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Filesystem paths the skill needs access to (outside the project sandbox).
    /// Values may include `~` for home-directory expansion and `discover:` prefixes
    /// for client-side auto-discovery at install time.
    #[serde(rename = "allowed-paths", skip_serializing_if = "Option::is_none")]
    pub allowed_paths: Option<Vec<String>>,
    /// Shell commands the skill needs to run (e.g. `obsidian-cli`).
    #[serde(rename = "allowed-commands", skip_serializing_if = "Option::is_none")]
    pub allowed_commands: Option<Vec<String>>,
}

/// A fully loaded skill with frontmatter, markdown body, and provenance.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Parsed YAML frontmatter.
    pub frontmatter: SkillFrontmatter,
    /// Markdown body after the frontmatter (the actual instructions).
    pub body: String,
    /// Where this skill was loaded from.
    pub source: SkillSource,
    /// Filesystem directory containing the SKILL.md.
    pub dir_path: PathBuf,
}

/// Where a skill was loaded from — determines override precedence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// `{workspace}/skills/` — highest precedence.
    Workspace,
    /// `~/.aura/agents/{agent_id}/skills/`.
    AgentPersonal,
    /// `~/.aura/skills/`.
    Personal,
    /// Shipped with the runtime.
    Bundled,
    /// An arbitrary extra directory from config.
    Extra(PathBuf),
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Workspace => write!(f, "workspace"),
            Self::AgentPersonal => write!(f, "agent_personal"),
            Self::Personal => write!(f, "personal"),
            Self::Bundled => write!(f, "bundled"),
            Self::Extra(p) => write!(f, "extra:{}", p.display()),
        }
    }
}

impl SkillSource {
    /// Numeric precedence (higher number overrides lower).
    #[must_use]
    pub const fn precedence(&self) -> u8 {
        match self {
            Self::Workspace => 5,
            Self::AgentPersonal => 4,
            Self::Personal => 3,
            Self::Bundled => 1,
            Self::Extra(_) => 2,
        }
    }
}

/// Lightweight metadata suitable for prompt injection and UI listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// Skill name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Source location.
    pub source: SkillSource,
    /// Whether the model can invoke this skill autonomously.
    pub model_invocable: bool,
    /// Whether the user can invoke this skill directly.
    pub user_invocable: bool,
    /// Filesystem paths the skill requests access to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_paths: Vec<String>,
    /// Shell commands the skill requests access to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_commands: Vec<String>,
}

/// Result of activating (rendering) a skill with concrete arguments.
#[derive(Debug, Clone)]
pub struct SkillActivation {
    /// Name of the activated skill.
    pub skill_name: String,
    /// Fully rendered content with all substitutions applied.
    pub rendered_content: String,
    /// Tool allowlist (empty = all tools allowed).
    pub allowed_tools: Vec<String>,
    /// Whether the skill requested a forked context.
    pub fork_context: bool,
    /// Sub-agent type if specified in the frontmatter.
    pub agent_type: Option<String>,
}
