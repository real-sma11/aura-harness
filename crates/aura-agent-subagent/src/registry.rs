//! Bundled foreground subagent registry.
//!
//! V1 intentionally uses fixed Rust data. Custom file-backed agents can layer
//! on this API later without changing the `task` tool contract.
//!
//! Phase B / Commit 3 / Step 3a relocated this module from
//! `aura-runtime/src/subagent_registry.rs` so the agent layer owns the
//! pure-data subagent surface. No fleet deps are added — the
//! `aura-core` types are sufficient.

use aura_core_types::{Capability, SubagentBudget, SubagentKindSpec};

const READ_TOOLS: &[&str] = &[
    "list_files",
    "read_file",
    "stat_file",
    "find_files",
    "search_code",
];

/// Lookup table for bundled subagent kinds.
#[derive(Debug, Clone)]
pub struct SubagentRegistry {
    kinds: Vec<SubagentKindSpec>,
}

impl SubagentRegistry {
    #[must_use]
    pub fn from_specs(kinds: Vec<SubagentKindSpec>) -> Self {
        Self { kinds }
    }

    #[must_use]
    pub fn bundled() -> Self {
        Self {
            kinds: vec![general_purpose(), explore(), shell(), code_reviewer()],
        }
    }

    #[must_use]
    pub fn all(&self) -> &[SubagentKindSpec] {
        &self.kinds
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SubagentKindSpec> {
        self.kinds.iter().find(|kind| kind.name == name)
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::bundled()
    }
}

/// Approximate the on-prompt character cost of a [`SubagentRegistry`]
/// for the per-turn context breakdown. Counts the name, description,
/// system prompt, and the comma-joined `allowed_tools` list — the
/// fields the dispatch tool surfaces back to the parent agent so it
/// knows what each subagent kind can do. Other fields (budgets,
/// capabilities) carry negligible token cost and are ignored.
#[must_use]
pub fn registry_chars(registry: &SubagentRegistry) -> usize {
    registry
        .all()
        .iter()
        .map(|kind| {
            let tools_chars = kind
                .allowed_tools
                .iter()
                .map(|t| t.len() + 1) // +1 per join separator
                .sum::<usize>();
            kind.name.len() + kind.description.len() + kind.system_prompt.len() + tools_chars
        })
        .sum()
}

/// Render each subagent kind into a `(name, text)` pair for the
/// per-turn context *contents* viewer (parallel to [`registry_chars`],
/// which produces the token count for the same surface). `text` joins
/// the same fields [`registry_chars`] accounts for — description,
/// system prompt, and the `allowed_tools` list — so the rendered text
/// and its token estimate describe the same bytes.
#[must_use]
pub fn registry_segments(registry: &SubagentRegistry) -> Vec<(String, String)> {
    registry
        .all()
        .iter()
        .map(|kind| {
            let tools = kind.allowed_tools.join(", ");
            let text = format!(
                "{}\n\n{}\n\nTools: {}",
                kind.description, kind.system_prompt, tools
            );
            (kind.name.clone(), text)
        })
        .collect()
}

fn general_purpose() -> SubagentKindSpec {
    SubagentKindSpec {
        name: "general_purpose".into(),
        description: "General-purpose subagent for multi-step codebase work.".into(),
        system_prompt: "You are a focused coding subagent. Complete the delegated task and return a concise final summary.".into(),
        allowed_tools: vec![
            "list_files".into(),
            "read_file".into(),
            "stat_file".into(),
            "find_files".into(),
            "search_code".into(),
            "edit_file".into(),
            "write_file".into(),
            "delete_file".into(),
            "run_command".into(),
        ],
        allowed_capabilities: vec![Capability::ReadAgent, Capability::InvokeProcess],
        readonly: false,
        default_model: None,
        budget: SubagentBudget {
            max_iterations: aura_core_types::MAX_TURNS,
            max_tokens: None,
            timeout_ms: 300_000,
        },
    }
}

fn explore() -> SubagentKindSpec {
    readonly_kind(
        "explore",
        "Read-only codebase exploration subagent.",
            "Explore the codebase using read/search tools and safe verification commands only. Return relevant files, symbols, and conclusions.",
    )
}

fn code_reviewer() -> SubagentKindSpec {
    readonly_kind(
        "code_reviewer",
        "Read-only code review subagent focused on bugs, regressions, and missing tests.",
        "Review the requested code for correctness risks. Lead with findings and cite concrete files.",
    )
}

fn shell() -> SubagentKindSpec {
    SubagentKindSpec {
        name: "shell".into(),
        description: "Command-focused subagent with tightly configured shell access.".into(),
        system_prompt:
            "Run only the requested safe shell/read operations and summarize the result.".into(),
        allowed_tools: READ_TOOLS
            .iter()
            .copied()
            .chain(std::iter::once("run_command"))
            .map(str::to_string)
            .collect(),
        allowed_capabilities: vec![Capability::InvokeProcess],
        readonly: false,
        default_model: None,
        budget: SubagentBudget {
            max_iterations: aura_core_types::MAX_TURNS,
            max_tokens: None,
            timeout_ms: 180_000,
        },
    }
}

fn readonly_kind(name: &str, description: &str, system_prompt: &str) -> SubagentKindSpec {
    SubagentKindSpec {
        name: name.into(),
        description: description.into(),
        system_prompt: system_prompt.into(),
        allowed_tools: READ_TOOLS
            .iter()
            .copied()
            .chain(std::iter::once("run_command"))
            .map(str::to_string)
            .collect(),
        allowed_capabilities: vec![Capability::InvokeProcess],
        readonly: true,
        default_model: None,
        budget: SubagentBudget {
            max_iterations: aura_core_types::MAX_TURNS,
            max_tokens: None,
            timeout_ms: 180_000,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `registry_chars` must be > 0 for the bundled registry so the
    /// per-turn context breakdown surfaces a non-zero "Subagents"
    /// bucket out of the box. Concrete value isn't asserted because
    /// the bundled prompts are free-text and may evolve.
    #[test]
    fn registry_chars_is_nonzero_for_bundled_registry() {
        let registry = SubagentRegistry::bundled();
        assert!(registry_chars(&registry) > 0);
    }

    #[test]
    fn registry_chars_is_zero_for_empty_registry() {
        let registry = SubagentRegistry::from_specs(Vec::new());
        assert_eq!(registry_chars(&registry), 0);
    }

    /// `registry_segments` must yield one labeled, non-empty segment per
    /// bundled kind so the context-contents viewer shows the same
    /// surface `registry_chars` counts tokens for.
    #[test]
    fn registry_segments_cover_every_bundled_kind() {
        let registry = SubagentRegistry::bundled();
        let segments = registry_segments(&registry);
        assert_eq!(segments.len(), registry.all().len());
        for name in ["general_purpose", "explore", "shell", "code_reviewer"] {
            let seg = segments
                .iter()
                .find(|(label, _)| label == name)
                .unwrap_or_else(|| panic!("missing segment for {name}"));
            assert!(!seg.1.is_empty(), "{name} segment text should be non-empty");
        }
    }

    #[test]
    fn registry_segments_is_empty_for_empty_registry() {
        let registry = SubagentRegistry::from_specs(Vec::new());
        assert!(registry_segments(&registry).is_empty());
    }

    #[test]
    fn bundled_registry_contains_expected_kinds() {
        let registry = SubagentRegistry::bundled();
        for name in ["general_purpose", "explore", "shell", "code_reviewer"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
        assert_eq!(registry.all().len(), 4);
    }

    #[test]
    fn unknown_kind_is_denied_by_lookup() {
        let registry = SubagentRegistry::bundled();
        assert!(registry.get("made_up").is_none());
    }

    #[test]
    fn readonly_kinds_have_no_file_mutation_tools() {
        let registry = SubagentRegistry::bundled();
        for name in ["explore", "code_reviewer"] {
            let kind = registry.get(name).unwrap();
            assert!(kind.readonly);
            for denied in ["write_file", "edit_file", "delete_file"] {
                assert!(
                    !kind.allowed_tools.iter().any(|tool| tool == denied),
                    "{name} unexpectedly allows {denied}"
                );
            }
            assert!(kind.allowed_tools.iter().any(|tool| tool == "run_command"));
            assert!(
                kind.allowed_capabilities
                    .iter()
                    .any(|capability| *capability == Capability::InvokeProcess),
                "{name} must retain InvokeProcess for verification commands"
            );
        }
    }

    #[test]
    fn run_command_kinds_request_invoke_process_capability() {
        let registry = SubagentRegistry::bundled();
        for kind in registry.all() {
            if !kind.allowed_tools.iter().any(|tool| tool == "run_command") {
                continue;
            }
            assert!(
                kind.allowed_capabilities
                    .iter()
                    .any(|capability| *capability == Capability::InvokeProcess),
                "{} exposes run_command without InvokeProcess",
                kind.name
            );
        }
    }
}
