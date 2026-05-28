//! Build verification and auto-fix loop.
//!
//! Provides [`verify_and_fix_build`] which runs a build command, detects
//! failures, requests LLM-generated fixes, and iterates until the build
//! passes or retries are exhausted.

use std::collections::HashSet;
use std::path::Path;

use tracing::{info, warn};

use crate::file_ops::FileOp;

use super::common::apply_fix_and_record;
use super::error_types::BuildFixAttemptRecord;
use super::runner;
use super::signatures::{normalize_error_signature, parse_individual_error_signatures};
use super::test::run_and_handle_tests;
use super::utils::{
    all_errors_in_baseline, auto_correct_build_command, infer_default_build_command,
    rollback_to_snapshot, snapshot_modified_files, FileSnapshot,
};
use super::{emit, FixProvider, VerifyConfig, VerifyEvent};

/// Parameters for [`verify_and_fix_build`].
pub struct BuildVerifyParams<'a> {
    pub project_root: &'a Path,
    pub build_command: Option<&'a str>,
    pub test_command: Option<&'a str>,
    /// Label used in log messages (e.g. task title).
    pub task_label: &'a str,
    pub baseline_build_errors: &'a HashSet<String>,
    /// File ops from the initial execution, used to snapshot before fix loop.
    pub initial_file_ops: &'a [FileOp],
}

/// Result of [`verify_and_fix_build`].
pub struct BuildVerifyResult {
    pub fix_ops: Vec<FileOp>,
    pub build_passed: bool,
    pub attempts_used: u32,
    pub duplicate_bailouts: u32,
    pub fix_input_tokens: u64,
    pub fix_output_tokens: u64,
    pub last_stderr: String,
}

/// Run the build command once and return the set of pre-existing error
/// signatures for use as a baseline.
pub async fn capture_build_baseline(
    project_root: &Path,
    build_command: Option<&str>,
) -> HashSet<String> {
    let cmd = match build_command {
        Some(cmd) if !cmd.trim().is_empty() => cmd.to_string(),
        _ => match infer_default_build_command(project_root) {
            Some(cmd) => cmd,
            None => return HashSet::new(),
        },
    };
    match runner::run_build_command(project_root, &cmd, None).await {
        Ok(result) if !result.success => {
            let errors = parse_individual_error_signatures(&result.stderr);
            if !errors.is_empty() {
                info!(
                    count = errors.len(),
                    "captured {} pre-existing build error(s) as baseline",
                    errors.len()
                );
            }
            errors
        }
        _ => HashSet::new(),
    }
}

fn resolve_build_command(
    project_root: &Path,
    build_command: Option<&str>,
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<VerifyEvent>>,
) -> Option<String> {
    let cmd = match build_command {
        Some(cmd) if !cmd.trim().is_empty() => cmd.to_string(),
        _ => {
            if let Some(fallback) = infer_default_build_command(project_root) {
                info!(
                    command = %fallback,
                    "build_command missing; using inferred safe default for verification"
                );
                return Some(fallback);
            }
            emit(
                event_tx,
                VerifyEvent::BuildSkipped {
                    reason: "no build_command configured".into(),
                },
            );
            return None;
        }
    };
    let mut build_command = cmd;
    if let Some(corrected) = auto_correct_build_command(&build_command) {
        warn!(
            old = %build_command, new = %corrected,
            "eagerly rewriting server-starting build command"
        );
        build_command = corrected;
    }
    Some(build_command)
}

fn check_error_stagnation(
    task_label: &str,
    stderr: &str,
    prior_attempts: &[BuildFixAttemptRecord],
    attempt: u32,
) -> bool {
    let current_signature = normalize_error_signature(stderr);
    let consecutive_dupes = prior_attempts
        .iter()
        .rev()
        .take_while(|a| a.error_signature == current_signature)
        .count();
    if consecutive_dupes >= 2 {
        info!(
            task = %task_label, attempt,
            "same error pattern repeated {} times, aborting fix loop",
            consecutive_dupes + 1
        );
        return true;
    }
    false
}

/// Mutable state for the verify-and-fix loop.
struct FixLoopState {
    fix_ops: Vec<FileOp>,
    prior: Vec<BuildFixAttemptRecord>,
    test_prior: Vec<BuildFixAttemptRecord>,
    dup_bail: u32,
    inp_t: u64,
    out_t: u64,
    last_stderr: String,
}

impl FixLoopState {
    fn to_result(&self, build_passed: bool, attempts_used: u32) -> BuildVerifyResult {
        BuildVerifyResult {
            fix_ops: self.fix_ops.clone(),
            build_passed,
            attempts_used,
            duplicate_bailouts: self.dup_bail,
            fix_input_tokens: self.inp_t,
            fix_output_tokens: self.out_t,
            last_stderr: if build_passed {
                String::new()
            } else {
                self.last_stderr.clone()
            },
        }
    }
}

/// Run the build/test verify-and-fix loop.
///
/// Iterates up to `config.max_build_fix_retries`, running the build command,
/// requesting LLM fixes on failure, and optionally running tests on success.
pub async fn verify_and_fix_build(
    params: &BuildVerifyParams<'_>,
    config: &VerifyConfig,
    fix_provider: &dyn FixProvider,
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<VerifyEvent>>,
) -> anyhow::Result<BuildVerifyResult> {
    let mut build_cmd =
        match resolve_build_command(params.project_root, params.build_command, event_tx) {
            Some(cmd) => cmd,
            None => {
                return Ok(BuildVerifyResult {
                    fix_ops: vec![],
                    build_passed: true,
                    attempts_used: 0,
                    duplicate_bailouts: 0,
                    fix_input_tokens: 0,
                    fix_output_tokens: 0,
                    last_stderr: String::new(),
                });
            }
        };

    let base_path = params.project_root;
    let pre_fix_snapshots = snapshot_modified_files(base_path, params.initial_file_ops);
    let mut st = FixLoopState {
        fix_ops: Vec::new(),
        prior: Vec::new(),
        test_prior: Vec::new(),
        dup_bail: 0,
        inp_t: 0,
        out_t: 0,
        last_stderr: String::new(),
    };

    for attempt in 1..=config.max_build_fix_retries {
        let br = run_build_step(base_path, &build_cmd, event_tx).await?;

        if br.timed_out {
            if let Some(c) = auto_correct_build_command(&build_cmd) {
                warn!(old = %build_cmd, new = %c, "build command timed out, auto-correcting");
                build_cmd = c;
                continue;
            }
        }

        if br.success {
            return handle_build_success(
                params,
                &build_cmd,
                &br,
                attempt,
                fix_provider,
                event_tx,
                &mut st,
            )
            .await;
        }

        let should_return = handle_build_failure(
            BuildAttemptCtx {
                params,
                build_cmd: &build_cmd,
                br: &br,
                config,
                pre_fix_snapshots: &pre_fix_snapshots,
                fix_provider,
                event_tx,
            },
            attempt,
            &mut st,
        )
        .await?;

        if let Some(result) = should_return {
            return Ok(result);
        }
    }

    Ok(st.to_result(false, config.max_build_fix_retries))
}

async fn run_build_step(
    base_path: &Path,
    build_cmd: &str,
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<VerifyEvent>>,
) -> anyhow::Result<runner::BuildResult> {
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Some(tx) = event_tx {
        let fwd = tx.clone();
        tokio::spawn(async move {
            while let Some(line) = line_rx.recv().await {
                let _ = fwd.send(VerifyEvent::OutputDelta(line));
            }
        });
    } else {
        tokio::spawn(async move { while line_rx.recv().await.is_some() {} });
    }

    emit(
        event_tx,
        VerifyEvent::BuildStarted {
            command: build_cmd.to_string(),
        },
    );

    Ok(runner::run_build_command(base_path, build_cmd, Some(line_tx)).await?)
}

async fn handle_build_success(
    params: &BuildVerifyParams<'_>,
    build_cmd: &str,
    br: &runner::BuildResult,
    attempt: u32,
    fix_provider: &dyn FixProvider,
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<VerifyEvent>>,
    st: &mut FixLoopState,
) -> anyhow::Result<BuildVerifyResult> {
    let dur = 0u64; // timing already logged by run_build_step
    emit(
        event_tx,
        VerifyEvent::BuildPassed {
            command: build_cmd.to_string(),
            stdout: br.stdout.clone(),
            duration_ms: dur,
        },
    );
    match params.test_command {
        Some(test_cmd) if !test_cmd.trim().is_empty() => {
            let (tp, i, o) = run_and_handle_tests(
                params.project_root,
                test_cmd,
                attempt,
                fix_provider,
                event_tx,
                &mut st.test_prior,
                &mut st.fix_ops,
            )
            .await?;
            st.inp_t += i;
            st.out_t += o;
            if tp {
                return Ok(st.to_result(true, attempt));
            }
            Ok(st.to_result(false, attempt))
        }
        _ => Ok(st.to_result(true, attempt)),
    }
}

/// Phase 8 wrapper carrying the per-attempt borrows that
/// [`handle_build_failure`] consumes alongside the running
/// [`FixLoopState`]. Bundling these four references keeps the
/// helper under the clippy ceiling without dragging the long-lived
/// state into the same struct (it stays `&mut` for in-place
/// mutation).
struct BuildAttemptCtx<'a> {
    params: &'a BuildVerifyParams<'a>,
    build_cmd: &'a str,
    br: &'a runner::BuildResult,
    config: &'a VerifyConfig,
    pre_fix_snapshots: &'a [FileSnapshot],
    fix_provider: &'a dyn FixProvider,
    event_tx: Option<&'a tokio::sync::mpsc::UnboundedSender<VerifyEvent>>,
}

async fn handle_build_failure(
    ctx: BuildAttemptCtx<'_>,
    attempt: u32,
    st: &mut FixLoopState,
) -> anyhow::Result<Option<BuildVerifyResult>> {
    let BuildAttemptCtx {
        params,
        build_cmd,
        br,
        config,
        pre_fix_snapshots,
        fix_provider,
        event_tx,
    } = ctx;
    st.last_stderr.clone_from(&br.stderr);
    emit(
        event_tx,
        VerifyEvent::BuildFailed {
            command: build_cmd.to_string(),
            stdout: br.stdout.clone(),
            stderr: br.stderr.clone(),
            attempt,
            duration_ms: 0,
        },
    );

    if all_errors_in_baseline(params.baseline_build_errors, &br.stderr) {
        return Ok(Some(st.to_result(true, attempt)));
    }

    if attempt == config.max_build_fix_retries {
        info!(task = %params.task_label, "build still failing after max retries");
        return Ok(Some(st.to_result(false, attempt)));
    }

    if check_error_stagnation(params.task_label, &br.stderr, &st.prior, attempt) {
        st.dup_bail += 1;
        rollback_to_snapshot(params.project_root, pre_fix_snapshots).await;
        info!(task = %params.task_label, "rolled back files after stagnated fix loop");
        return Ok(Some(st.to_result(false, attempt)));
    }

    emit(event_tx, VerifyEvent::BuildFixAttempt { attempt });

    let (response, i, o) = fix_provider
        .request_fix(build_cmd, &br.stderr, &br.stdout, &st.prior)
        .await?;
    st.inp_t += i;
    st.out_t += o;

    apply_fix_and_record(
        params.project_root,
        super::common::FixAttempt {
            response: &response,
            attempt,
            stderr: &br.stderr,
            fix_kind: "build-fix",
        },
        &mut st.prior,
        &mut st.fix_ops,
        fix_provider,
    )
    .await?;

    Ok(None)
}
