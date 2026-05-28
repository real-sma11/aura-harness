//! End-to-end provision + teardown of a real `git worktree`.
//!
//! Skipped gracefully when `git` is not on `PATH` so CI on minimal
//! Windows / container images doesn't fail the suite. Local dev
//! machines (and the default CI image) have `git` available, so the
//! happy path runs there.

use std::path::Path;
use std::process::Command;

use aura_exec_isolation::{Isolation, IsolationStrategy, WorktreeIsolation};
use tempfile::TempDir;

fn git_available() -> bool {
    which::which("git").is_ok()
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .expect("git binary must launch");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

#[test]
fn worktree_provision_then_teardown() {
    if !git_available() {
        eprintln!("skipping worktree test: git not on PATH");
        return;
    }

    let repo = TempDir::new().unwrap();
    let parent = TempDir::new().unwrap();

    // Configure a self-contained repo with one commit so `HEAD`
    // resolves. The local user.* config keeps `git commit` working in
    // sandboxed CI runners that don't carry global git identity.
    run_git(repo.path(), &["init", "--quiet", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@example.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    std::fs::write(repo.path().join("README.md"), b"seed\n").unwrap();
    run_git(repo.path(), &["add", "README.md"]);
    run_git(repo.path(), &["commit", "--quiet", "-m", "seed"]);

    // `repo.path()` may be a relative-looking symlink target on some
    // OSes — canonicalize so the absolute-path invariant holds.
    let canonical_repo = repo.path().canonicalize().unwrap();
    let canonical_parent = parent.path().canonicalize().unwrap();

    let iso = WorktreeIsolation::new(canonical_parent.clone());
    let ws = iso
        .provision(&canonical_repo, "child-1")
        .expect("worktree provision must succeed");
    assert_eq!(ws.strategy, IsolationStrategy::Worktree);
    assert!(ws.root.is_absolute());
    assert!(ws.root.exists());
    assert!(ws.root.join("README.md").exists());

    iso.teardown(&ws).expect("worktree teardown must not error");
    // `git worktree remove -f` deletes the checkout directory; the
    // parent stays.
    assert!(
        !ws.root.exists(),
        "worktree dir should be gone after teardown"
    );
}
