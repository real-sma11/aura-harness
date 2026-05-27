//! Task context assembly and budget management.
//!
//! Gathers codebase context, dependency APIs, type definitions, and
//! conventions, then assembles them into a single context string that
//! fits within a configurable token budget.
//
// TODO(phase5, 2026-04-22): task-context helpers are staged for Phase 5
// integration with the task-aware tool executor. Until then, silence
// dead-code warnings at the module root rather than per-symbol.
#![allow(dead_code)]

use std::path::Path;

use crate::file_ops::{self, WorkspaceCache};
use crate::prompts::TaskInfo;

// ---------------------------------------------------------------------------
// Codebase context fetching
// ---------------------------------------------------------------------------

/// Gathered codebase context for a task.
pub struct CodebaseContext {
    pub codebase_snapshot: String,
    pub dep_api_context: String,
    pub type_defs_context: String,
}

/// Fetch task-relevant codebase files, dependency APIs, and type definitions.
///
/// Uses the [`WorkspaceCache`] for fast lookups where available.
pub async fn fetch_codebase_context(
    project_folder: &str,
    task_title: &str,
    task_description: &str,
    spec_contents: &str,
    workspace_cache: &WorkspaceCache,
    workspace_map: &str,
) -> CodebaseContext {
    let codebase_snapshot = match file_ops::retrieve_task_relevant_files_cached(
        project_folder,
        task_title,
        task_description,
        50_000,
        workspace_cache,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cached file retrieval failed, falling back to basic read: {e}");
            let folder = project_folder.to_string();
            tokio::task::spawn_blocking(move || {
                file_ops::read_relevant_files(&folder, 50_000).unwrap_or_else(|e2| {
                    tracing::warn!("fallback read_relevant_files also failed: {e2}");
                    String::new()
                })
            })
            .await
            .unwrap_or_else(|e2| {
                tracing::warn!("spawn_blocking panicked: {e2}");
                String::new()
            })
        }
    };

    let dep_api_context = if workspace_map.is_empty() {
        String::new()
    } else {
        file_ops::resolve_task_dep_api_context_cached(
            project_folder,
            task_title,
            task_description,
            15_000,
            workspace_cache,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("resolve_task_dep_api_context_cached failed: {e}");
            String::new()
        })
    };

    let type_defs_context = file_ops::type_resolution::resolve_type_definitions_for_task_async(
        project_folder,
        task_title,
        task_description,
        spec_contents,
        10_000,
    )
    .await;

    CodebaseContext {
        codebase_snapshot,
        dep_api_context,
        type_defs_context,
    }
}

// ---------------------------------------------------------------------------
// Context assembly
// ---------------------------------------------------------------------------

pub use aura_config::{MAX_TASK_CONTEXT_CHARS, MAX_WORK_LOG_TASK_CONTEXT};

/// Compose the full task context from the base context and codebase sections.
///
/// Appends workspace structure, type definitions, codebase snapshot, and
/// dependency API sections, then caps the total to [`MAX_TASK_CONTEXT_CHARS`].
pub fn build_full_task_context(
    mut task_context: String,
    workspace_map: &str,
    type_defs: &str,
    codebase_snapshot: &str,
    dep_api: &str,
) -> String {
    let conventions = extract_codebase_conventions(codebase_snapshot);
    if !conventions.is_empty() {
        task_context.push_str(&conventions);
    }
    if !workspace_map.is_empty() {
        task_context.push_str(&format!("\n# Workspace Structure\n{workspace_map}\n"));
    }
    if !type_defs.is_empty() {
        task_context.push_str(&format!(
            "\n# Type Definitions Referenced in Task\n{type_defs}\n",
        ));
    }
    if !codebase_snapshot.is_empty() {
        task_context.push_str(&format!(
            "\n# Current Codebase Files\n{codebase_snapshot}\n",
        ));
    }
    if !dep_api.is_empty() {
        task_context.push_str(&format!("\n# Dependency API Surface\n{dep_api}\n"));
    }
    cap_task_context(&mut task_context, MAX_TASK_CONTEXT_CHARS);
    task_context
}

pub fn cap_bootstrap_task_context(task_context: &mut String) {
    let budget = aura_config::agent().prompts.bootstrap_context_chars;
    if budget == 0 || task_context.len() <= budget {
        return;
    }

    cap_task_context(task_context, budget);
    if task_context.len() <= budget {
        return;
    }

    let marker = "\n\n[... task context truncated before LLM routing; use read_file/search_code/get_task_context for additional details ...]\n";
    let marker_len = marker.len().min(budget);
    let keep = budget.saturating_sub(marker_len);
    let cut = task_context
        .char_indices()
        .take_while(|(idx, ch)| idx + ch.len_utf8() <= keep)
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    task_context.truncate(cut);
    task_context.push_str(marker);
}

/// Build a summary of the work log for inclusion in task context.
pub fn build_work_log_summary(work_log: &[String]) -> String {
    if work_log.is_empty() {
        return String::new();
    }
    let mut summary = work_log.join("\n---\n");
    if summary.len() > MAX_WORK_LOG_TASK_CONTEXT {
        summary.truncate(MAX_WORK_LOG_TASK_CONTEXT);
        summary.push_str("\n... (truncated) ...");
    }
    summary
}

// ---------------------------------------------------------------------------
// Convention detection
// ---------------------------------------------------------------------------

fn extract_codebase_conventions(codebase_snapshot: &str) -> String {
    let mut conventions = Vec::new();

    if codebase_snapshot.contains("thiserror") {
        conventions.push("Error types: uses thiserror derive macros");
    }
    if codebase_snapshot.contains("#[tokio::test]") {
        conventions.push("Tests: async tests with #[tokio::test]");
    }
    if codebase_snapshot.contains("Arc<") && codebase_snapshot.contains("impl") {
        conventions.push("Services: wrapped in Arc for shared ownership");
    }
    if codebase_snapshot.contains("tracing::") || codebase_snapshot.contains("use tracing") {
        conventions.push("Logging: uses tracing crate");
    }
    if codebase_snapshot.contains("serde::") || codebase_snapshot.contains("#[derive(Serialize") {
        conventions.push("Serialization: uses serde with derive macros");
    }
    if codebase_snapshot.contains("async fn") && codebase_snapshot.contains("await") {
        conventions.push("Async: uses async/await patterns");
    }
    if codebase_snapshot.contains("#[cfg(test)]") {
        conventions.push("Tests: inline #[cfg(test)] modules");
    }

    if conventions.is_empty() {
        return String::new();
    }
    format!(
        "\n# Codebase Conventions (follow these patterns)\n{}\n",
        conventions
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

// ---------------------------------------------------------------------------
// Context budget management
// ---------------------------------------------------------------------------

/// Trim `task_context` to at most `budget` characters by progressively
/// removing lower-priority sections (codebase snapshot first, then dep API,
/// then workspace map), preserving the core task description and spec.
const CAP_SECTIONS: &[&str] = &[
    "\n# Current Codebase Files\n",
    "\n# Dependency API Surface\n",
    "\n# Workspace Structure\n",
    "\n# Type Definitions Referenced in Task\n",
];

pub fn cap_task_context(task_context: &mut String, budget: usize) {
    if task_context.len() <= budget {
        return;
    }

    for section_header in CAP_SECTIONS {
        if task_context.len() <= budget {
            return;
        }
        if let Some(start) = task_context.find(section_header) {
            let next_section = task_context[start + section_header.len()..]
                .find("\n# ")
                .map(|pos| start + section_header.len() + pos);
            let end = next_section.unwrap_or(task_context.len());

            let section_len = end - start;
            let overshoot = task_context.len().saturating_sub(budget);

            if overshoot >= section_len {
                task_context.replace_range(start..end, "");
            } else {
                let keep = section_len - overshoot;
                let trim_start = start + keep;
                task_context.replace_range(
                    trim_start..end,
                    "\n... (truncated to fit context budget) ...\n",
                );
            }
        }
    }

    if task_context.len() > budget {
        task_context.truncate(budget);
        task_context.push_str("\n... (context truncated) ...\n");
    }
}

// ---------------------------------------------------------------------------
// Redundancy check
// ---------------------------------------------------------------------------

/// Conservative pre-check: skip simple tasks whose deliverables already exist
/// in the workspace (e.g. a struct/module defined by a predecessor task).
///
/// Returns a reason string if the task appears to be already completed.
pub async fn check_already_completed(
    project_folder: &str,
    task: &TaskInfo<'_>,
    completed_deps: &[TaskInfo<'_>],
) -> Option<String> {
    if completed_deps.is_empty() {
        return None;
    }

    let desc_lower = format!("{} {}", task.title, task.description).to_lowercase();
    let base = Path::new(project_folder);

    let define_patterns: &[(&str, &str)] = &[
        ("define struct ", "struct "),
        ("define enum ", "enum "),
        ("define type ", "type "),
        ("create struct ", "struct "),
        ("create enum ", "enum "),
    ];
    for (trigger, code_prefix) in define_patterns {
        if let Some(pos) = desc_lower.find(trigger) {
            let after = &desc_lower[pos + trigger.len()..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                continue;
            }

            let dep_files: Vec<&str> = completed_deps
                .iter()
                .flat_map(|d| d.files_changed.iter())
                .map(|f| f.path.as_str())
                .collect();

            for file_path in &dep_files {
                let full_path = base.join(file_path);
                if let Ok(content) = tokio::fs::read_to_string(&full_path).await {
                    let needle = format!("{code_prefix}{name}");
                    if content.to_lowercase().contains(&needle.to_lowercase()) {
                        return Some(format!(
                            "`{code_prefix}{name}` already exists in {file_path} (created by a predecessor task)",
                        ));
                    }
                }
            }
        }
    }

    None
}

/// Resolve completed dependency tasks from a flat list.
///
/// Filters `all_tasks` to those whose ID appears in `dependency_ids` and
/// whose status indicates completion (non-empty `execution_notes`).
pub fn resolve_completed_deps<'a>(
    all_tasks: &'a [TaskInfo<'a>],
    dependency_ids: &[&str],
) -> Vec<&'a TaskInfo<'a>> {
    if dependency_ids.is_empty() {
        return Vec::new();
    }
    all_tasks
        .iter()
        .filter(|t| dependency_ids.contains(&t.title) && !t.execution_notes.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_work_log_summary_empty() {
        assert_eq!(build_work_log_summary(&[]), "");
    }

    #[test]
    fn build_work_log_summary_joins_entries() {
        let log = vec!["Task 1 done".into(), "Task 2 done".into()];
        let summary = build_work_log_summary(&log);
        assert!(summary.contains("Task 1 done"));
        assert!(summary.contains("---"));
        assert!(summary.contains("Task 2 done"));
    }

    #[test]
    fn build_work_log_summary_truncates_long_input() {
        let log: Vec<String> = (0..500)
            .map(|i| format!("Entry {i}: some work done here"))
            .collect();
        let summary = build_work_log_summary(&log);
        assert!(summary.len() <= MAX_WORK_LOG_TASK_CONTEXT + 30);
        assert!(summary.contains("(truncated)"));
    }

    #[test]
    fn cap_task_context_within_budget_unchanged() {
        let mut ctx = "Short context".to_string();
        let original = ctx.clone();
        cap_task_context(&mut ctx, 1000);
        assert_eq!(ctx, original);
    }

    #[test]
    fn cap_task_context_trims_codebase_section_first() {
        let mut ctx = String::new();
        ctx.push_str("# Task\nDo something\n");
        ctx.push_str("\n# Current Codebase Files\n");
        ctx.push_str(&"x".repeat(5000));
        ctx.push_str("\n# Dependency API Surface\n");
        ctx.push_str("dep info here");

        let original_len = ctx.len();
        let budget = 200;
        cap_task_context(&mut ctx, budget);
        assert!(ctx.len() < original_len);
        assert!(!ctx.contains(&"x".repeat(4000)));
        assert!(ctx.contains("truncated"));
    }

    #[test]
    fn cap_task_context_hard_truncate_last_resort() {
        let mut ctx = "x".repeat(10_000);
        cap_task_context(&mut ctx, 500);
        assert!(ctx.len() <= 550);
        assert!(ctx.contains("(context truncated)"));
    }

    #[test]
    fn cap_bootstrap_task_context_applies_small_routing_budget() {
        let mut ctx = String::new();
        ctx.push_str("# Task\nFix the issue\n");
        ctx.push_str("\n# Current Codebase Files\n");
        ctx.push_str(&"x".repeat(20_000));

        cap_bootstrap_task_context(&mut ctx);

        assert!(
            ctx.len() <= aura_config::DEFAULT_BOOTSTRAP_TASK_CONTEXT_CHARS,
            "expected bootstrap context <= {}, got {}",
            aura_config::DEFAULT_BOOTSTRAP_TASK_CONTEXT_CHARS,
            ctx.len()
        );
        assert!(ctx.contains("# Task"));
    }

    #[test]
    fn test_extract_codebase_conventions_empty_input() {
        assert_eq!(extract_codebase_conventions(""), String::new());
    }

    #[test]
    fn test_extract_codebase_conventions_no_matches() {
        assert_eq!(
            extract_codebase_conventions("fn main() { println!(\"hello\"); }"),
            String::new(),
        );
    }

    #[test]
    fn test_extract_codebase_conventions_all_conventions() {
        let input = r#"
            use thiserror;
            #[tokio::test]
            Arc<SomeService>
            impl Foo {}
            tracing::info!("hi");
            #[derive(Serialize, Deserialize)]
            async fn do_work() { something.await }
            #[cfg(test)]
        "#;
        let result = extract_codebase_conventions(input);
        assert!(result.contains("thiserror"));
        assert!(result.contains("tokio::test"));
        assert!(result.contains("Arc"));
        assert!(result.contains("tracing"));
        assert!(result.contains("serde"));
        assert!(result.contains("async"));
        assert!(result.contains("#[cfg(test)]"));
        let convention_count = result.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(convention_count, 7);
    }

    #[test]
    fn test_build_full_task_context_appends_all_sections() {
        let result = build_full_task_context(
            "base".to_string(),
            "workspace map",
            "type defs",
            "codebase snap",
            "dep api",
        );
        assert!(result.contains("Workspace Structure"));
        assert!(result.contains("Type Definitions"));
        assert!(result.contains("Current Codebase Files"));
        assert!(result.contains("Dependency API Surface"));
    }

    #[test]
    fn test_build_full_task_context_empty_extras_stays_minimal() {
        let result = build_full_task_context("base".to_string(), "", "", "", "");
        assert_eq!(result, "base");
    }
}
