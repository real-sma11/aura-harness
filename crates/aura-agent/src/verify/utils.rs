//! File snapshot/rollback utilities, build command inference, and context
//! snapshot helpers for build-fix prompts.

use std::collections::HashSet;
use std::path::Path;

use tracing::{info, warn};

use crate::file_ops::{self, FileOp};

use super::error_types::parse_error_references;
use super::signatures::parse_individual_error_signatures;

/// Pre-fix file content captured for rollback on stagnation.
pub struct FileSnapshot {
    pub path: String,
    pub content: Option<String>,
}

/// Snapshot the current on-disk content of every file touched by `file_ops`.
pub fn snapshot_modified_files(project_root: &Path, file_ops: &[FileOp]) -> Vec<FileSnapshot> {
    let mut seen = HashSet::new();
    let mut snapshots = Vec::new();
    for op in file_ops {
        let path = match op {
            FileOp::Create { path, .. }
            | FileOp::Modify { path, .. }
            | FileOp::SearchReplace { path, .. }
            | FileOp::Delete { path } => path,
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let full_path = project_root.join(path);
        let content = std::fs::read_to_string(&full_path).ok();
        snapshots.push(FileSnapshot {
            path: path.clone(),
            content,
        });
    }
    snapshots
}

/// Restore files to a previously captured snapshot.
pub async fn rollback_to_snapshot(project_root: &Path, snapshots: &[FileSnapshot]) {
    for snap in snapshots {
        let full_path = project_root.join(&snap.path);
        match &snap.content {
            Some(content) => {
                if let Err(e) = tokio::fs::write(&full_path, content).await {
                    warn!(path = %snap.path, error = %e, "failed to rollback file");
                }
            }
            None => {
                if let Err(e) = tokio::fs::remove_file(&full_path).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!(path = %snap.path, error = %e, "failed to delete file during rollback");
                    }
                }
            }
        }
    }
}

/// Rewrite known server-starting commands to their build/check equivalents.
pub fn auto_correct_build_command(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();
    if trimmed == "cargo run" || trimmed.starts_with("cargo run ") {
        let mut corrected = trimmed.replacen("cargo run", "cargo build", 1);
        if let Some(idx) = corrected.find(" -- ") {
            corrected.truncate(idx);
        } else if corrected.ends_with(" --") {
            corrected.truncate(corrected.len() - 3);
        }
        return Some(corrected);
    }
    if trimmed == "npm start" {
        return Some("npm run build".to_string());
    }
    if trimmed.contains("runserver") {
        return Some(trimmed.replace("runserver", "check"));
    }
    None
}

/// Infer a default build-check command from project manifest files.
pub fn infer_default_build_command(project_root: &Path) -> Option<String> {
    if project_root.join("Cargo.toml").is_file() {
        return Some("cargo check --workspace --tests".to_string());
    }
    if project_root.join("package.json").is_file() {
        return Some("npm run build --if-present".to_string());
    }
    if project_root.join("pyproject.toml").is_file()
        || project_root.join("requirements.txt").is_file()
    {
        return Some("python -m compileall .".to_string());
    }
    None
}

/// Infer one deterministic default test command from project manifest files.
///
/// Recognises Cargo, the JS package managers (Bun, pnpm, Yarn, npm, Deno),
/// Python (pytest), Go, Ruby (Bundler/RSpec), Maven, Gradle, and .NET. The
/// inference deliberately returns a single command in a stable precedence
/// order instead of auto-chaining every detected ecosystem. Polyglot projects
/// that need a full multi-suite gate should provide an explicit test command.
///
/// Returns `None` when no recognised manifest is present; the caller should
/// then decide whether to skip the test gate (analysis-only project) or
/// abort with a configuration error.
pub fn infer_default_test_command(project_root: &Path) -> Option<String> {
    if project_root.join("Cargo.toml").is_file() {
        return Some("cargo test --workspace --all-features".to_string());
    }

    // JS/TS ecosystem: pick the runner the project actually uses by
    // looking at the lockfile, then fall back to npm. Only one JS runner
    // wins because they all execute the same `package.json` scripts.
    if project_root.join("package.json").is_file() {
        return Some(if project_root.join("bun.lockb").is_file()
            || project_root.join("bun.lock").is_file()
        {
            "bun test"
        } else if project_root.join("pnpm-lock.yaml").is_file() {
            "pnpm test --if-present --silent"
        } else if project_root.join("yarn.lock").is_file() {
            "yarn test --silent"
        } else {
            "npm test --silent --if-present"
        }
        .to_string());
    }

    if project_root.join("deno.json").is_file() || project_root.join("deno.jsonc").is_file() {
        // Deno projects don't carry a package.json by default.
        return Some("deno test --quiet".to_string());
    }

    if project_root.join("pyproject.toml").is_file()
        || project_root.join("requirements.txt").is_file()
        || project_root.join("setup.py").is_file()
        || project_root.join("setup.cfg").is_file()
        || project_root.join("pytest.ini").is_file()
    {
        return Some("python -m pytest -q".to_string());
    }

    if project_root.join("go.mod").is_file() {
        return Some("go test ./...".to_string());
    }

    if project_root.join("Gemfile").is_file() {
        // Prefer rspec when there's a spec/ directory; otherwise default
        // to `bundle exec rake test` which is the convention for plain
        // Test::Unit / Minitest projects.
        let ruby_cmd = if project_root.join("spec").is_dir() {
            "bundle exec rspec"
        } else {
            "bundle exec rake test"
        };
        return Some(ruby_cmd.to_string());
    }

    if project_root.join("pom.xml").is_file() {
        return Some("mvn -B test".to_string());
    } else if project_root.join("build.gradle").is_file()
        || project_root.join("build.gradle.kts").is_file()
        || project_root.join("settings.gradle").is_file()
        || project_root.join("settings.gradle.kts").is_file()
    {
        let wrapper = if project_root.join("gradlew").is_file()
            || project_root.join("gradlew.bat").is_file()
        {
            "./gradlew"
        } else {
            "gradle"
        };
        return Some(format!("{wrapper} test"));
    }

    if has_dotnet_project(project_root) {
        return Some("dotnet test --nologo".to_string());
    }

    None
}

/// Detect a .NET project by searching for `*.sln`, `*.csproj`, or `*.fsproj`
/// at the top level. Cheap heuristic — doesn't recurse, which matches how
/// `dotnet test` walks its own solution graph.
fn has_dotnet_project(project_root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            return false;
        };
        matches!(ext, "sln" | "csproj" | "fsproj" | "vbproj")
    })
}

/// Build a codebase snapshot for a build-fix prompt by reading error source
/// files fresh from disk and supplementing with relevant project files.
pub fn build_error_context_snapshot(
    project_root: &Path,
    build_stderr: &str,
    budget: usize,
) -> String {
    let error_refs = parse_error_references(build_stderr);
    let fresh_error_files = file_ops::resolve_error_source_files(project_root, &error_refs, budget);

    if fresh_error_files.is_empty() {
        file_ops::read_relevant_files(&project_root.display().to_string(), budget)
            .unwrap_or_default()
    } else {
        let remaining_budget = budget.saturating_sub(fresh_error_files.len());
        if remaining_budget > 2_000 {
            let supplemental = file_ops::read_relevant_files(
                &project_root.display().to_string(),
                remaining_budget,
            )
            .unwrap_or_default();
            if supplemental.is_empty() {
                fresh_error_files
            } else {
                format!("{fresh_error_files}\n{supplemental}")
            }
        } else {
            fresh_error_files
        }
    }
}

/// Returns true if all current errors are pre-existing (present in baseline).
pub fn all_errors_in_baseline(baseline: &HashSet<String>, stderr: &str) -> bool {
    if baseline.is_empty() {
        return false;
    }
    let current_errors = parse_individual_error_signatures(stderr);
    if current_errors.is_empty() {
        return false;
    }
    let new_errors: HashSet<_> = current_errors.difference(baseline).cloned().collect();
    if new_errors.is_empty() {
        info!(
            pre_existing = current_errors.len(),
            "all build errors are pre-existing (baseline), treating as passed"
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "").unwrap();
    }

    #[test]
    fn infer_test_command_for_cargo_workspace() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("Cargo.toml"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.starts_with("cargo test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_picks_pnpm_over_npm_via_lockfile() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("package.json"));
        touch(&dir.path().join("pnpm-lock.yaml"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        // Use word-boundary checks: "pnpm test" naturally contains
        // "npm test" as a substring, so split into tokens to assert
        // the leading runner name is exactly `pnpm`, not `npm`.
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(tokens.first().copied(), Some("pnpm"), "got {cmd}");
        assert_eq!(tokens.get(1).copied(), Some("test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_picks_yarn_via_lockfile() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("package.json"));
        touch(&dir.path().join("yarn.lock"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("yarn test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_picks_bun_via_lockfile() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("package.json"));
        touch(&dir.path().join("bun.lockb"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("bun test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_defaults_to_npm_without_lockfile() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("package.json"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("npm test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_deno() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("deno.json"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("deno test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_python_pyproject() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("pyproject.toml"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("pytest"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_go() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("go.mod"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert_eq!(cmd, "go test ./...");
    }

    #[test]
    fn infer_test_command_for_ruby_with_spec_dir() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("Gemfile"));
        fs::create_dir_all(dir.path().join("spec")).unwrap();
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("rspec"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_ruby_without_spec_dir_uses_rake() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("Gemfile"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("rake test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_maven() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("pom.xml"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("mvn") && cmd.contains("test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_gradle_with_wrapper() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("build.gradle.kts"));
        touch(&dir.path().join("gradlew"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("./gradlew test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_for_dotnet() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("MyApp.csproj"));
        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert!(cmd.contains("dotnet test"), "got {cmd}");
    }

    #[test]
    fn infer_test_command_uses_first_recognized_manifest_for_polyglot_project() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("Cargo.toml"));
        touch(&dir.path().join("package.json"));
        touch(&dir.path().join("pyproject.toml"));
        touch(&dir.path().join("go.mod"));

        let cmd = infer_default_test_command(dir.path()).unwrap();
        assert_eq!(cmd, "cargo test --workspace --all-features");
        assert!(
            !cmd.contains(" && "),
            "inferred defaults must not auto-chain: {cmd}"
        );
    }

    #[test]
    fn infer_test_command_returns_none_on_empty_project() {
        let dir = tempdir().unwrap();
        assert!(infer_default_test_command(dir.path()).is_none());
    }
}
