//! Build command execution and test output parsing.
//!
//! Provides [`run_build_command`] for executing shell commands with streaming
//! output, timeout handling, and output truncation. Also includes parsers for
//! cargo test, Jest, and generic test output formats.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

/// Typed errors returned by [`run_build_command`] and its helpers.
///
/// Library-crate signature replacement for the previous
/// `anyhow::Result<_>` returns. `anyhow::Error: From<E>` for any
/// `E: std::error::Error + 'static` so binary call sites still get
/// the same `?` ergonomics.
#[derive(Debug, thiserror::Error)]
pub enum BuildRunnerError {
    /// Caller passed an empty command string.
    #[error("build_command is empty")]
    EmptyCommand,
    /// `Command::spawn` failed.
    #[error("failed to execute build command `{command}` in `{cwd}`: {source}")]
    SpawnFailed {
        /// The literal command string.
        command: String,
        /// Working directory it was invoked in.
        cwd: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// IO error while reading a child process pipe.
    #[error("failed reading build command {stream}: {source}")]
    PipeRead {
        /// `"stdout"` or `"stderr"`.
        stream: &'static str,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// IO error while waiting for the child process.
    #[error("IO error waiting for build command `{command}`: {source}")]
    Wait {
        /// The literal command string.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// A spawned `tokio::task::JoinHandle` failed.
    #[error("build command `{command}` {stream} collector failed: {source}")]
    CollectorJoin {
        /// The literal command string.
        command: String,
        /// `"stdout"` or `"stderr"`.
        stream: String,
        /// Underlying join error.
        #[source]
        source: tokio::task::JoinError,
    },
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Convenience alias for the join handle returned by the output
/// collector tasks.
type CollectorHandle = tokio::task::JoinHandle<Result<String, BuildRunnerError>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndividualTestResult {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

use aura_config::MAX_OUTPUT_BYTES;
const BUILD_TIMEOUT: Duration = Duration::from_secs(aura_config::BUILD_TIMEOUT_SECS);

fn truncate_output(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let start = &s[..floor_char_boundary(s, half)];
    let end = &s[ceil_char_boundary(s, s.len() - half)..];
    format!(
        "{start}\n\n... (truncated {0} bytes) ...\n\n{end}",
        s.len() - max
    )
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn needs_shell(cmd: &str) -> bool {
    cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains('<')
        || cmd.contains(';')
        || cmd.contains('$')
        || cmd.contains('`')
}

/// Run a build command in the project directory and capture the result.
///
/// Simple commands are split on whitespace and executed directly. Commands
/// containing shell operators (`&&`, `|`, etc.) are run through the system
/// shell (`cmd /C` on Windows, `sh -c` on Unix).
///
/// If `output_tx` is provided, stdout/stderr lines are streamed through
/// the channel as they arrive.
pub async fn run_build_command(
    project_dir: &Path,
    build_command: &str,
    output_tx: Option<UnboundedSender<String>>,
) -> Result<BuildResult, BuildRunnerError> {
    if build_command.split_whitespace().next().is_none() {
        return Err(BuildRunnerError::EmptyCommand);
    }

    info!(
        dir = %project_dir.display(),
        command = %build_command,
        "running build verification"
    );

    let mut child = spawn_build_child(project_dir, build_command)?;

    let (stdout_handle, stderr_handle) = spawn_output_collectors(&mut child, output_tx);

    let result =
        await_build_result(&mut child, build_command, stdout_handle, stderr_handle).await?;

    log_build_result(&result, build_command);
    Ok(result)
}

fn spawn_build_child(
    project_dir: &Path,
    build_command: &str,
) -> Result<tokio::process::Child, BuildRunnerError> {
    let child = if needs_shell(build_command) {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let mut c = Command::new("cmd");
            c.as_std_mut().raw_arg(format!("/C {build_command}"));
            c.current_dir(project_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Command::new("sh")
                .args(["-c", build_command])
                .current_dir(project_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
    } else {
        let parts: Vec<&str> = build_command.split_whitespace().collect();
        // On Windows, Rust's `Command::new("npm")` ignores `PATHEXT` so
        // node-ecosystem shims (`npm.cmd`, `yarn.cmd`, `pnpm.cmd`,
        // `vite.cmd`, etc.) fail with `program not found` even when they
        // are clearly on PATH. Resolve the bare name through `which`
        // first (which *does* honour `PATHEXT`) so the build/test gate
        // can run npm scripts on Windows without forcing operators to
        // wrap every command in `cmd /C`.
        #[cfg(target_os = "windows")]
        let program: std::ffi::OsString = windows_resolve_program(parts[0], project_dir)
            .map_or_else(|| parts[0].into(), std::path::PathBuf::into_os_string);
        #[cfg(not(target_os = "windows"))]
        let program: std::ffi::OsString = parts[0].into();

        Command::new(&program)
            .args(&parts[1..])
            .current_dir(project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
    .map_err(|e| BuildRunnerError::SpawnFailed {
        command: build_command.to_string(),
        cwd: project_dir.display().to_string(),
        source: e,
    })?;
    Ok(child)
}

/// PATHEXT-aware bare-name resolution used by [`spawn_build_child`] on
/// Windows. See the analogous helper in
/// `aura_tools::fs_tools::cmd` — kept duplicated here to avoid widening
/// `aura-tools`' public surface for what's a small, very local fix.
///
/// Returns `None` for inputs that already look like a path or carry an
/// explicit executable extension; the caller then falls back to passing
/// the original string straight to `Command::new`, which preserves the
/// pre-fix behaviour (and lets `Command::new` produce the canonical OS
/// `not found` error if the file truly doesn't exist).
#[cfg(target_os = "windows")]
fn windows_resolve_program(program: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return None;
    }
    let lower = program.to_ascii_lowercase();
    if lower.ends_with(".exe")
        || lower.ends_with(".cmd")
        || lower.ends_with(".bat")
        || lower.ends_with(".com")
        || lower.ends_with(".ps1")
    {
        return None;
    }
    let path_env = std::env::var("PATH").ok();
    match path_env {
        Some(p) => which::which_in(program, Some(p), cwd).ok(),
        None => which::which(program).ok(),
    }
}

fn spawn_output_collectors(
    child: &mut tokio::process::Child,
    output_tx: Option<UnboundedSender<String>>,
) -> (CollectorHandle, CollectorHandle) {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_tx = output_tx.clone();
    let stdout_handle =
        tokio::spawn(async move { collect_lines("stdout", stdout_pipe, stdout_tx).await });
    let stderr_handle =
        tokio::spawn(async move { collect_lines("stderr", stderr_pipe, output_tx).await });

    (stdout_handle, stderr_handle)
}

async fn collect_lines<R: tokio::io::AsyncRead + Unpin>(
    stream_name: &'static str,
    pipe: Option<R>,
    tx: Option<UnboundedSender<String>>,
) -> Result<String, BuildRunnerError> {
    let mut collected = String::new();
    if let Some(pipe) = pipe {
        let mut reader = tokio::io::BufReader::new(pipe).lines();
        while let Some(line) = reader
            .next_line()
            .await
            .map_err(|e| BuildRunnerError::PipeRead {
                stream: stream_name,
                source: e,
            })?
        {
            if let Some(ref tx) = tx {
                let _ = tx.send(format!("{line}\n"));
            }
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    Ok(collected)
}

async fn await_build_result(
    child: &mut tokio::process::Child,
    build_command: &str,
    stdout_handle: CollectorHandle,
    stderr_handle: CollectorHandle,
) -> Result<BuildResult, BuildRunnerError> {
    match tokio::time::timeout(BUILD_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout_raw = collect_joined_output(stdout_handle, "stdout", build_command).await?;
            let stderr_raw = collect_joined_output(stderr_handle, "stderr", build_command).await?;
            Ok(BuildResult {
                success: status.success(),
                stdout: truncate_output(&stdout_raw, MAX_OUTPUT_BYTES),
                stderr: truncate_output(&stderr_raw, MAX_OUTPUT_BYTES),
                exit_code: status.code(),
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(BuildRunnerError::Wait {
            command: build_command.to_string(),
            source: e,
        }),
        Err(_) => handle_build_timeout(child, build_command, stdout_handle, stderr_handle).await,
    }
}

async fn collect_joined_output(
    handle: CollectorHandle,
    stream_name: &str,
    build_command: &str,
) -> Result<String, BuildRunnerError> {
    handle.await.map_err(|e| BuildRunnerError::CollectorJoin {
        command: build_command.to_string(),
        stream: stream_name.to_string(),
        source: e,
    })?
}

async fn handle_build_timeout(
    child: &mut tokio::process::Child,
    build_command: &str,
    stdout_handle: CollectorHandle,
    stderr_handle: CollectorHandle,
) -> Result<BuildResult, BuildRunnerError> {
    warn!(
        command = %build_command,
        timeout_secs = BUILD_TIMEOUT.as_secs(),
        "build command timed out, killing process"
    );
    if let Err(e) = child.kill().await {
        warn!(command = %build_command, error = %e, "failed to kill timed-out build process");
    }
    let partial_stderr = collect_timeout_output(stderr_handle, "stderr", build_command).await;
    let timeout_msg = format!(
        "Build command `{build_command}` timed out after {}s. The command may start a long-running \
         process (e.g. a server). Use `cargo build` or `cargo check` instead of \
         `cargo run` for build verification.",
        BUILD_TIMEOUT.as_secs()
    );
    let stderr = if partial_stderr.is_empty() {
        timeout_msg
    } else {
        format!(
            "{}\n\n{}",
            truncate_output(&partial_stderr, MAX_OUTPUT_BYTES),
            timeout_msg
        )
    };
    Ok(BuildResult {
        success: false,
        stdout: truncate_output(
            &collect_timeout_output(stdout_handle, "stdout", build_command).await,
            MAX_OUTPUT_BYTES,
        ),
        stderr,
        exit_code: None,
        timed_out: true,
    })
}

async fn collect_timeout_output(
    handle: CollectorHandle,
    stream_name: &str,
    build_command: &str,
) -> String {
    match collect_joined_output(handle, stream_name, build_command).await {
        Ok(output) => output,
        Err(e) => format!("[failed to collect build command {stream_name}: {e}]\n"),
    }
}

fn log_build_result(result: &BuildResult, build_command: &str) {
    if result.success {
        info!(command = %build_command, "build verification passed");
    } else {
        // Logged at `info!` rather than `warn!` because a failed build
        // here is *not* a regression on its own — the build-baseline
        // machinery in `agent_loop::tool_pipeline::run_auto_build`
        // and `BuildBaseline::annotate` is what decides whether this
        // failure represents *new* errors versus matching the
        // pre-existing baseline. That layer surfaces real regressions
        // at `warn` level; the runner itself should stay quiet so
        // workspaces with a known-dirty baseline don't spam a `WARN`
        // after every write-tool invocation (observed in harness runs
        // as identical `stderr_len=10212` lines on every tool call).
        info!(
            command = %build_command,
            exit_code = ?result.exit_code,
            stderr_len = result.stderr.len(),
            "build verification returned non-zero \
             (baseline comparison done by agent_loop::run_auto_build)"
        );
    }
}

/// Parse test runner output into individual test results and a summary line.
///
/// Recognises cargo test, Jest/Vitest/Mocha-style JS runners, pytest, Go
/// `go test`, and RSpec. Each parser returns an empty vec when its format
/// signature isn't present, so the chain falls through to the next one. If
/// nothing matches, a single aggregate result is synthesised from the exit
/// code so unparseable output still flows through the DoD test gate.
///
/// The DoD hard gate combines this parser with the runner's exit code: a
/// suite is considered passing only when `success == true` AND no parsed
/// failure is present. That means a runner whose output we don't recognise
/// still gates correctly via its exit code, while runners we *do* recognise
/// also surface failing test names back to the agent so it can fix the
/// right thing.
pub fn parse_test_output(
    stdout: &str,
    stderr: &str,
    success: bool,
) -> (Vec<IndividualTestResult>, String) {
    let combined = format!("{stdout}\n{stderr}");

    for parser in [
        parse_cargo_test as fn(&str) -> Vec<IndividualTestResult>,
        parse_jest_output,
        parse_pytest_output,
        parse_go_test_output,
        parse_rspec_output,
    ] {
        let results = parser(&combined);
        if !results.is_empty() {
            return (results.clone(), tally_summary(&results));
        }
    }

    let status = if success { "passed" } else { "failed" };
    let summary = if success {
        "all tests passed".to_string()
    } else {
        "tests failed".to_string()
    };
    let result = IndividualTestResult {
        name: "(aggregate)".to_string(),
        status: status.to_string(),
        message: if success {
            None
        } else {
            Some(truncate_output(&combined, 2000))
        },
    };
    (vec![result], summary)
}

fn tally_summary(results: &[IndividualTestResult]) -> String {
    let passed = results.iter().filter(|r| r.status == "passed").count();
    let failed = results.iter().filter(|r| r.status == "failed").count();
    let skipped = results.iter().filter(|r| r.status == "skipped").count();
    if skipped > 0 {
        format!("{passed} passed, {failed} failed, {skipped} skipped")
    } else {
        format!("{passed} passed, {failed} failed")
    }
}

fn parse_cargo_test(output: &str) -> Vec<IndividualTestResult> {
    let mut results = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("test ") {
            continue;
        }
        let rest = &trimmed[5..];
        if let Some(idx) = rest.find(" ... ") {
            let name = rest[..idx].trim().to_string();
            let outcome = rest[idx + 5..].trim();
            let status = match outcome {
                "ok" => "passed",
                "FAILED" => "failed",
                s if s.starts_with("ignored") => "skipped",
                _ => continue,
            };
            let message = if status == "failed" {
                Some(outcome.to_string())
            } else {
                None
            };
            results.push(IndividualTestResult {
                name,
                status: status.to_string(),
                message,
            });
        }
    }
    results
}

fn parse_jest_output(output: &str) -> Vec<IndividualTestResult> {
    let mut results = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("PASS ") {
            results.push(IndividualTestResult {
                name: rest.trim().to_string(),
                status: "passed".to_string(),
                message: None,
            });
        } else if let Some(rest) = trimmed.strip_prefix("FAIL ") {
            results.push(IndividualTestResult {
                name: rest.trim().to_string(),
                status: "failed".to_string(),
                message: None,
            });
        } else if trimmed.starts_with("\u{2713} ") || trimmed.starts_with("✓ ") {
            results.push(IndividualTestResult {
                name: trimmed[2..].trim().to_string(),
                status: "passed".to_string(),
                message: None,
            });
        } else if trimmed.starts_with("\u{2717} ")
            || trimmed.starts_with("✕ ")
            || trimmed.starts_with("✗ ")
        {
            results.push(IndividualTestResult {
                name: trimmed[3..].trim().to_string(),
                status: "failed".to_string(),
                message: None,
            });
        }
    }
    results
}

/// Parse pytest output. Pytest emits two relevant formats:
///
/// * Verbose live output (`pytest -v`):
///   `tests/foo.py::test_x PASSED                          [ 25%]`
///   — the test ID is *first*, the status word is in the middle.
/// * Short-form summary block (`pytest -q` and the trailing summary
///   of any verbose run):
///   `FAILED tests/foo.py::test_x - AssertionError: ...`
///   — the status word is *first*, the test ID follows.
///
/// We try both shapes per line and dedupe by test ID so a verbose run
/// that emits the same failure in both the live log and the summary
/// block still counts as one failure. Lines that don't contain a
/// `pytest::node` style ID are ignored to avoid grabbing unrelated
/// `FAILED to load shared lib` chatter from build noise.
fn parse_pytest_output(output: &str) -> Vec<IndividualTestResult> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        let Some((status, name)) = parse_pytest_line(trimmed) else {
            continue;
        };
        if !looks_like_pytest_node(&name) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        results.push(IndividualTestResult {
            name,
            status: status.to_string(),
            message: None,
        });
    }

    results
}

fn parse_pytest_line(line: &str) -> Option<(&'static str, String)> {
    // Shape 1: status keyword at the start. Used in pytest's tail
    // summary block and by `pytest -rf` style flag rendering.
    if let Some((status, rest)) = strip_pytest_status_prefix(line) {
        let name = rest.split(" - ").next().unwrap_or(rest).trim().to_string();
        if !name.is_empty() {
            return Some((status, name));
        }
    }

    // Shape 2: status keyword at the end of the line (possibly
    // followed by `[ NN% ]`). Used by `pytest -v`.
    if let Some((status, name)) = strip_pytest_status_suffix(line) {
        return Some((status, name));
    }

    None
}

fn strip_pytest_status_prefix(line: &str) -> Option<(&'static str, &str)> {
    for (kw, status) in PYTEST_KEYWORDS {
        if let Some(rest) = line.strip_prefix(kw).and_then(|r| r.strip_prefix(' ')) {
            return Some((status, rest));
        }
    }
    None
}

fn strip_pytest_status_suffix(line: &str) -> Option<(&'static str, String)> {
    // Drop the trailing percent indicator if present: `... PASSED [ 25%]`.
    let body = if let Some(idx) = line.rfind('[') {
        let trailing = line[idx..].trim();
        if trailing.ends_with("%]") {
            line[..idx].trim_end()
        } else {
            line
        }
    } else {
        line
    };

    for (kw, status) in PYTEST_KEYWORDS {
        if let Some(prefix) = body.strip_suffix(kw) {
            if prefix.ends_with(char::is_whitespace) {
                let name = prefix.trim().to_string();
                if !name.is_empty() {
                    return Some((status, name));
                }
            }
        }
    }
    None
}

const PYTEST_KEYWORDS: &[(&str, &str)] = &[
    ("PASSED", "passed"),
    ("FAILED", "failed"),
    ("SKIPPED", "skipped"),
    ("ERROR", "failed"),
    ("XFAIL", "skipped"),
    ("XPASS", "passed"),
];

/// Pytest test IDs always contain `::` (e.g. `tests/foo.py::test_x` or
/// `tests/foo.py::Class::test_x`). Filter on this so we don't snag
/// unrelated `FAILED ...` lines from non-pytest log output.
fn looks_like_pytest_node(name: &str) -> bool {
    name.contains("::")
}

/// Parse `go test` output. Recognises the `--- PASS:`, `--- FAIL:`,
/// `--- SKIP:` summary lines emitted by the standard testing package, plus
/// the per-package `FAIL\tpkg/...` lines that surface when a package fails
/// to build before any test runs.
fn parse_go_test_output(output: &str) -> Vec<IndividualTestResult> {
    const GO_PREFIXES: &[(&str, &str)] = &[
        ("--- PASS: ", "passed"),
        ("--- FAIL: ", "failed"),
        ("--- SKIP: ", "skipped"),
    ];

    let mut results = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();

        let parsed = GO_PREFIXES
            .iter()
            .find_map(|(prefix, status)| trimmed.strip_prefix(prefix).map(|rest| (*status, rest)));

        if let Some((status, rest)) = parsed {
            // `--- FAIL: TestName (0.12s)` → keep just `TestName`.
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                results.push(IndividualTestResult {
                    name,
                    status: status.to_string(),
                    message: None,
                });
            }
            continue;
        }

        // Build-time / package-level failures look like:
        //   `FAIL\tgithub.com/example/pkg [build failed]`
        // No individual test names available, but we still want the run
        // to register as failing.
        if let Some(rest) = trimmed.strip_prefix("FAIL\t") {
            let name = rest.split_whitespace().next().unwrap_or(rest);
            if !name.is_empty() {
                results.push(IndividualTestResult {
                    name: format!("{name} (package)"),
                    status: "failed".to_string(),
                    message: None,
                });
            }
        }
    }
    results
}

/// Parse RSpec's progress output. Looks for the failure summary block
/// (`Failures:` followed by `1) <full description>`) and counts examples
/// from the trailing `N examples, M failures, K pending` line.
fn parse_rspec_output(output: &str) -> Vec<IndividualTestResult> {
    let mut results = Vec::new();
    let mut in_failures_block = false;
    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed == "Failures:" {
            in_failures_block = true;
            continue;
        }
        if in_failures_block {
            if let Some(rest) = trimmed
                .strip_prefix(|c: char| c.is_ascii_digit())
                .and_then(|r| r.strip_prefix(')'))
            {
                let name = rest.trim().to_string();
                if !name.is_empty() {
                    results.push(IndividualTestResult {
                        name,
                        status: "failed".to_string(),
                        message: None,
                    });
                }
            }
            // The failures block ends when we hit a line starting with a
            // summary marker (`Finished in`, `N examples`, etc.).
            if trimmed.starts_with("Finished in") || trimmed.contains(" examples,") {
                in_failures_block = false;
            }
        }
    }

    // Synthesise pass entries from the summary line so a green RSpec run
    // also produces a non-empty parse result. Without this, an all-green
    // RSpec invocation would fall through to the aggregate-only branch.
    if let Some(stats) = rspec_summary_counts(output) {
        let already_failed = results.len();
        let passes = stats
            .examples
            .saturating_sub(stats.failures + stats.pending);
        for i in 0..passes {
            results.push(IndividualTestResult {
                name: format!("(rspec example #{idx})", idx = i + 1),
                status: "passed".to_string(),
                message: None,
            });
        }
        for i in 0..stats.pending {
            results.push(IndividualTestResult {
                name: format!("(rspec pending #{idx})", idx = i + 1),
                status: "skipped".to_string(),
                message: None,
            });
        }
        // Reconcile: if the summary reports more failures than the named
        // ones we captured, top up with anonymous entries so the gate
        // surfaces the right *count* even when failure descriptions
        // weren't parseable.
        let extra_fail = stats.failures.saturating_sub(already_failed);
        for i in 0..extra_fail {
            results.push(IndividualTestResult {
                name: format!("(rspec failure #{idx})", idx = already_failed + i + 1),
                status: "failed".to_string(),
                message: None,
            });
        }
    }

    results
}

#[derive(Default)]
struct RspecStats {
    examples: usize,
    failures: usize,
    pending: usize,
}

fn rspec_summary_counts(output: &str) -> Option<RspecStats> {
    for line in output.lines() {
        let trimmed = line.trim();
        // Looking for: `12 examples, 3 failures, 1 pending` (pending is
        // optional). Be lenient on whitespace.
        if !trimmed.contains(" examples,") {
            continue;
        }
        let mut stats = RspecStats::default();
        for chunk in trimmed.split(',') {
            let chunk = chunk.trim();
            let mut parts = chunk.split_whitespace();
            let Some(num) = parts.next().and_then(|s| s.parse::<usize>().ok()) else {
                continue;
            };
            match parts.next() {
                Some("examples") | Some("example") => stats.examples = num,
                Some("failures") | Some("failure") => stats.failures = num,
                Some("pending") => stats.pending = num,
                _ => {}
            }
        }
        if stats.examples > 0 || stats.failures > 0 {
            return Some(stats);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cargo_test_output() {
        let stdout = "\
running 3 tests
test utils::tests::test_parse ... ok
test utils::tests::test_format ... FAILED
test utils::tests::test_skip ... ignored
";
        let (results, summary) = parse_test_output(stdout, "", true);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].status, "passed");
        assert_eq!(results[1].status, "failed");
        assert_eq!(results[2].status, "skipped");
        assert!(summary.contains("1 passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 skipped"));
    }

    #[test]
    fn parse_jest_pass_fail() {
        let stdout = "\
PASS src/utils.test.ts
FAIL src/api.test.ts
PASS src/hooks.test.ts
";
        let (results, summary) = parse_test_output(stdout, "", true);
        assert_eq!(results.len(), 3);
        assert_eq!(results.iter().filter(|r| r.status == "passed").count(), 2);
        assert_eq!(results.iter().filter(|r| r.status == "failed").count(), 1);
        assert!(summary.contains("2 passed"));
    }

    #[test]
    fn parse_fallback_success() {
        let (results, summary) = parse_test_output("all ok", "", true);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "passed");
        assert!(summary.contains("all tests passed"));
    }

    #[test]
    fn parse_fallback_failure() {
        let (results, summary) = parse_test_output("boom", "something went wrong", false);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "failed");
        assert!(results[0].message.is_some());
        assert!(summary.contains("tests failed"));
    }

    #[test]
    fn truncate_short_output_unchanged() {
        assert_eq!(truncate_output("hello", 100), "hello");
    }

    #[test]
    fn truncate_long_output() {
        let long = "a".repeat(200);
        let result = truncate_output(&long, 50);
        assert!(result.len() < 200);
        assert!(result.contains("truncated"));
    }

    #[test]
    fn truncate_output_handles_multibyte_boundaries() {
        let long = "é".repeat(100);
        let result = truncate_output(&long, 51);
        assert!(result.contains("truncated"));
    }

    #[test]
    fn needs_shell_with_pipe() {
        assert!(needs_shell("cargo test | head"));
    }

    #[test]
    fn needs_shell_with_and() {
        assert!(needs_shell("cd foo && npm build"));
    }

    #[test]
    fn needs_shell_simple_command() {
        assert!(!needs_shell("cargo build --release"));
    }

    #[test]
    fn parse_pytest_output_short_form() {
        let stdout = "\
============================= test session starts ==============================
collected 4 items

tests/test_a.py::test_one PASSED
tests/test_a.py::test_two FAILED
tests/test_b.py::TestX::test_three PASSED
tests/test_c.py::test_four SKIPPED

=========================== short test summary info ============================
FAILED tests/test_a.py::test_two - AssertionError: expected 1 got 2
==================== 2 passed, 1 failed, 1 skipped in 0.12s ====================
";
        let (results, summary) = parse_test_output(stdout, "", false);
        let failed: Vec<_> = results.iter().filter(|r| r.status == "failed").collect();
        let passed: Vec<_> = results.iter().filter(|r| r.status == "passed").collect();
        let skipped: Vec<_> = results.iter().filter(|r| r.status == "skipped").collect();
        assert_eq!(passed.len(), 2);
        assert_eq!(failed.len(), 1, "summary block must not double-count");
        assert_eq!(skipped.len(), 1);
        assert_eq!(failed[0].name, "tests/test_a.py::test_two");
        assert!(summary.contains("1 failed"));
    }

    #[test]
    fn parse_go_test_output_pass_fail_skip() {
        let stdout = "\
=== RUN   TestAlpha
--- PASS: TestAlpha (0.01s)
=== RUN   TestBeta
--- FAIL: TestBeta (0.02s)
    foo_test.go:42: expected 5, got 4
=== RUN   TestGamma
--- SKIP: TestGamma (0.00s)
FAIL\tgithub.com/example/broken [build failed]
ok  \tgithub.com/example/ok\t0.123s
";
        let (results, summary) = parse_test_output(stdout, "", false);
        let names_failed: Vec<_> = results
            .iter()
            .filter(|r| r.status == "failed")
            .map(|r| r.name.clone())
            .collect();
        assert!(names_failed.iter().any(|n| n == "TestBeta"));
        assert!(
            names_failed
                .iter()
                .any(|n| n.contains("github.com/example/broken")),
            "package-level FAIL line should be captured: got {names_failed:?}"
        );
        assert!(results
            .iter()
            .any(|r| r.name == "TestAlpha" && r.status == "passed"));
        assert!(results
            .iter()
            .any(|r| r.name == "TestGamma" && r.status == "skipped"));
        assert!(summary.contains("failed"));
    }

    #[test]
    fn parse_rspec_output_with_failures() {
        let stdout = "\
Failures:

  1) UserModel#full_name returns first and last name
     Failure/Error: expect(user.full_name).to eq('Jane Doe')

  2) UserModel#email validates format
     Failure/Error: expect(user.errors).to be_empty

Finished in 0.234 seconds
12 examples, 2 failures, 1 pending
";
        let (results, summary) = parse_test_output(stdout, "", false);
        let failed: Vec<_> = results.iter().filter(|r| r.status == "failed").collect();
        let passed: Vec<_> = results.iter().filter(|r| r.status == "passed").collect();
        let skipped: Vec<_> = results.iter().filter(|r| r.status == "skipped").collect();
        assert_eq!(failed.len(), 2);
        assert_eq!(passed.len(), 12 - 2 - 1, "passes derived from summary line");
        assert_eq!(skipped.len(), 1);
        assert!(summary.contains("2 failed"));
    }

    #[test]
    fn parse_rspec_output_all_green() {
        let stdout = "\
Finished in 0.05 seconds
4 examples, 0 failures
";
        let (results, summary) = parse_test_output(stdout, "", true);
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r| r.status == "passed"));
        assert!(summary.contains("4 passed"));
    }

    #[test]
    fn parse_unknown_format_falls_back_to_exit_code() {
        let (results, _summary) = parse_test_output(
            "running tests with custom-runner v0.3 ...\nall green\n",
            "",
            true,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "(aggregate)");
        assert_eq!(results[0].status, "passed");

        let (results, _summary) = parse_test_output("custom-runner: failure!", "trace...", false);
        assert_eq!(results[0].status, "failed");
    }

    #[tokio::test]
    async fn run_build_command_rejects_empty_command() {
        let err = run_build_command(std::path::Path::new("."), "  ", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("build_command is empty"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn run_build_command_reports_spawn_failure_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let command = "__aura_missing_command_for_runner_test__";

        let err = run_build_command(dir.path(), command, None)
            .await
            .unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains(command), "missing command context: {msg}");
        assert!(
            msg.contains(&dir.path().display().to_string()),
            "missing directory context: {msg}"
        );
    }

    #[test]
    fn parse_pytest_does_not_eat_unrelated_failed_lines() {
        let stdout = "\
some build noise about FAILED to load shared lib
no test markers here
";
        let (results, _summary) = parse_test_output(stdout, "", true);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "(aggregate)");
    }

    /// `windows_resolve_program` must keep its hands off any input that
    /// already looks like a path or carries an explicit executable
    /// extension. Returning `None` here is the contract that lets
    /// `spawn_build_child` fall through to `Command::new(parts[0])` with
    /// the operator's original string, which is critical so we don't
    /// silently rewrite (for example) a project-local `./build.cmd`
    /// into a `.cmd` that happens to share a name on PATH.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_resolve_program_skips_paths_and_explicit_extensions() {
        use std::path::Path;

        let cwd = Path::new(".");
        // Anything containing a path separator → caller intent.
        assert!(windows_resolve_program(r"tools\foo.exe", cwd).is_none());
        assert!(windows_resolve_program("./build.sh", cwd).is_none());
        assert!(windows_resolve_program(r"C:\Windows\System32\where.exe", cwd).is_none());

        // Explicit extension → let `Command::new` do its own PATH search,
        // which already works for `.exe` and tolerates `.cmd`/`.bat`
        // when the operator typed them out.
        assert!(windows_resolve_program("npm.cmd", cwd).is_none());
        assert!(windows_resolve_program("BUILD.BAT", cwd).is_none());
        assert!(windows_resolve_program("python.exe", cwd).is_none());
    }

    /// Smoke test that `which::which_in` actually finds a real Windows
    /// shim when given a bare name. We probe `cmd` (which is always on
    /// PATH as `cmd.exe`) so the test is robust on stripped-down CI
    /// images that don't ship npm/node. The point is to assert *that*
    /// resolution happens, not which extension wins; on a host without
    /// `cmd` on PATH we skip rather than fail so this stays portable.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_resolve_program_finds_bare_name_via_pathext() {
        use std::path::Path;

        let cwd = Path::new(".");
        let Some(resolved) = windows_resolve_program("cmd", cwd) else {
            // `cmd` somehow not on PATH on this host. Nothing to assert
            // about behaviour we don't control; treat as a non-failure
            // skip so the test suite stays green in restrictive CI
            // sandboxes (e.g. WSL crates run on Linux containers).
            return;
        };
        assert!(
            resolved.is_absolute(),
            "expected an absolute resolved path, got {}",
            resolved.display()
        );
        let lower = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        assert!(
            lower.starts_with("cmd."),
            "expected resolved file to start with 'cmd.', got {lower}"
        );
    }
}
