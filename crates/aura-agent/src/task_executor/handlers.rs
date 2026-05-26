use super::{
    classify_build_errors, error_category_guidance, file_ops, format_tool_arg_hint,
    infer_default_test_command, looks_like_compiler_errors, AgentLoopEvent, FileOp,
    FollowUpSuggestion, Path, TaskPhase, TaskPlan, TaskToolExecutor, ToolCallInfo, ToolCallResult,
    MAX_STUB_FIX_ATTEMPTS,
};
use crate::prompts::{SteeringInjector, SteeringKind};

pub(super) fn enrich_compiler_output_sync(project_folder: &str, raw_output: &str) -> String {
    if !looks_like_compiler_errors(raw_output) {
        return raw_output.to_string();
    }

    let base_path = Path::new(project_folder);

    let categories = classify_build_errors(raw_output);
    let guidance = error_category_guidance(&categories);
    let refs = crate::verify::parse_error_references(raw_output);
    let api_ref = file_ops::resolve_error_context(base_path, &refs);

    let mut enriched = raw_output.to_string();

    if !guidance.is_empty() {
        enriched.push_str("\n\n## Error Diagnosis & Guidance\n\n");
        enriched.push_str(&guidance);
    }

    if !api_ref.is_empty() {
        enriched.push('\n');
        enriched.push_str(&api_ref);
    }

    enriched
}

impl TaskToolExecutor {
    fn tool_result(
        tc: &ToolCallInfo,
        content: impl Into<String>,
        is_error: bool,
        stop_loop: bool,
    ) -> ToolCallResult {
        ToolCallResult {
            tool_use_id: tc.id.clone(),
            content: content.into(),
            is_error,
            kind: if is_error {
                aura_core::ToolResultKind::AgentError
            } else {
                aura_core::ToolResultKind::Ok
            },
            stop_loop,
            file_changes: Vec::new(),
        }
    }

    fn gate_rejection(tc: &ToolCallInfo, content: impl Into<String>) -> ToolCallResult {
        Self::tool_result(tc, content, true, false)
    }

    pub(super) async fn track_file_op(&self, tool_name: &str, input: &serde_json::Value) {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return;
        }
        let op = match tool_name {
            "write_file" => {
                let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                FileOp::Create {
                    path: path.to_string(),
                    content: content.to_string(),
                }
            }
            "edit_file" => FileOp::Modify {
                path: path.to_string(),
                content: String::new(),
            },
            "delete_file" => FileOp::Delete {
                path: path.to_string(),
            },
            _ => return,
        };
        self.tracked_file_ops.lock().await.push(op);
    }

    pub(super) async fn handle_task_done(
        &self,
        tc: &ToolCallInfo,
        results: &mut Vec<ToolCallResult>,
        stop: &mut bool,
    ) {
        self.extract_notes_and_follow_ups(tc).await;

        if let Some(error_prompt) = self.check_pervasive_errors().await {
            results.push(Self::gate_rejection(tc, error_prompt));
            return;
        }

        if let Some(review_prompt) = self.check_self_review().await {
            results.push(Self::gate_rejection(tc, review_prompt));
            return;
        }

        if let Some(no_write_prompt) = self.check_no_writes().await {
            results.push(Self::gate_rejection(tc, no_write_prompt));
            return;
        }

        if let Some(stub_prompt) = self.check_stubs_and_reject().await {
            results.push(Self::gate_rejection(tc, stub_prompt));
            return;
        }

        if !self.should_skip_test_gate_for_no_change_completion().await {
            // Codex parity (May 2026): the project test suite is no
            // longer a hard gate. We still run it once, best-effort,
            // so the UI/operator sees the outcome — a failing run
            // emits a `TestSuiteWarning` event but does NOT block the
            // `task_done` success path or trigger a retry.
            self.run_test_suite_warning().await;
        }

        results.push(Self::tool_result(
            tc,
            r#"{"status":"completed"}"#,
            false,
            true,
        ));
        *stop = true;
    }

    pub(super) async fn extract_notes_and_follow_ups(&self, tc: &ToolCallInfo) {
        let task_notes = tc
            .input
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        {
            let mut n = self.notes.lock().await;
            *n = task_notes;
        }
        if let Some(arr) = tc.input.get("follow_ups").and_then(|v| v.as_array()) {
            let mut fu_lock = self.follow_ups.lock().await;
            for fu in arr {
                let title = fu
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let desc = fu
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                fu_lock.push(FollowUpSuggestion {
                    title,
                    description: desc,
                });
            }
        }
        if let Some(reasoning) = tc.input.get("reasoning").and_then(|v| v.as_array()) {
            let reasoning_text: Vec<String> = reasoning
                .iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect();
            if !reasoning_text.is_empty() {
                let mut n = self.notes.lock().await;
                n.push_str("\n\nReasoning:\n");
                for r in &reasoning_text {
                    n.push_str(&format!("- {r}\n"));
                }
            }
        }
        if tc
            .input
            .get("no_changes_needed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            *self.no_changes_needed.lock().await = true;
        }
    }

    async fn check_pervasive_errors(&self) -> Option<String> {
        let outcomes = self.recent_tool_outcomes.lock().await;
        if outcomes.last_command_failed {
            return Some(
                "ERROR: The last run_command exited non-zero. \
                 Your build or test is broken. Fix the errors before completing the task. \
                 (Policy-denied commands do not count — if run_command is blocked, rely on \
                 the harness's auto-build step and do not keep calling run_command.)"
                    .to_string(),
            );
        }
        let min_calls = 6;
        let error_threshold = 0.7;
        let total = outcomes.total();
        let real_errors = outcomes.real_errors();
        if total >= min_calls {
            #[allow(clippy::cast_precision_loss)]
            let error_ratio = real_errors as f64 / total as f64;
            if error_ratio >= error_threshold {
                return Some(format!(
                    "ERROR: {real_errors}/{total} recent tool calls returned errors \
                     ({:.0}% failure rate, policy denials excluded). The task is likely \
                     incomplete. Review the errors, fix the underlying issue, then try \
                     completing again.",
                    error_ratio * 100.0,
                ));
            }
        }
        None
    }

    async fn check_self_review(&self) -> Option<String> {
        let unreviewed = self.self_review.lock().await.check_review_needed()?;
        Some(format!(
            "SELF-REVIEW REQUIRED: Before completing, re-read the files you modified \
             to verify correctness:\n{}\n\nCheck: (a) changes match task requirements, \
             (b) no placeholder/stub code remains, (c) no debug code left behind.\n\
             Then call task_done again.",
            unreviewed.join("\n"),
        ))
    }

    async fn check_no_writes(&self) -> Option<String> {
        let ops = self.tracked_file_ops.lock().await;
        if !ops.is_empty() {
            return None;
        }
        let no_changes = *self.no_changes_needed.lock().await;
        if no_changes {
            return None;
        }
        self.recent_tool_outcomes
            .lock()
            .await
            .task_done_no_writes_rejected = true;
        Some(SteeringInjector::render(&SteeringKind::TaskDoneNoWrites))
    }

    async fn should_skip_test_gate_for_no_change_completion(&self) -> bool {
        if !*self.no_changes_needed.lock().await {
            return false;
        }
        self.tracked_file_ops.lock().await.is_empty()
    }

    /// Run the full project test suite once, best-effort, after the
    /// model called `task_done`. The outcome is surfaced as a
    /// [`AgentLoopEvent::TestSuiteWarning`] but never blocks the
    /// completion — see the Codex-parity note in `handle_task_done`.
    ///
    /// The command is resolved at call time, in priority order:
    ///   1. [`Self::test_command_override`] — operator-supplied via
    ///      [`super::TEST_COMMAND_OVERRIDE_ENV`] at executor construction.
    ///   2. [`Self::test_command`] — per-project configuration.
    ///   3. [`infer_default_test_command`] — manifest-driven auto-detect
    ///      (cargo, npm/pnpm/yarn/bun, deno, pytest, go, rspec/rake,
    ///      maven, gradle, dotnet — chained with `&&` for polyglot
    ///      monorepos).
    ///
    /// When all three return nothing the call no-ops with a debug
    /// log; analysis-only / doc-only projects continue to get clean
    /// `task_done` completions.
    pub(super) async fn run_test_suite_warning(&self) {
        let project_root = Path::new(&self.project_folder);
        let (cmd, source) = match self.resolve_test_command(project_root) {
            Some(resolved) => resolved,
            None => {
                tracing::debug!(
                    project = %self.project_folder,
                    "no test_command configured — skipping post-task_done test run"
                );
                return;
            }
        };

        self.emit_text(format!(
            "\n[post-task_done test run: {cmd} (source: {source})]\n"
        ));

        let outcome = match self.test_runner.run_tests(project_root, &cmd).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(error = %e, cmd, "post-task_done test run failed to execute");
                self.emit_event(AgentLoopEvent::TestSuiteWarning {
                    passed: false,
                    summary: format!("failed to execute `{cmd}`: {e}"),
                    failed_tests: Vec::new(),
                });
                return;
            }
        };

        if outcome.passed {
            self.emit_text(format!(
                "\n[post-task_done test run: PASSED in {ms}ms — {summary}]\n",
                ms = outcome.duration_ms,
                summary = outcome.summary,
            ));
        } else {
            self.emit_text(format!(
                "\n[post-task_done test run: FAILED — {summary}; \
                 task_done still succeeded (Codex-parity: warning, not gate)]\n",
                summary = outcome.summary,
            ));
        }

        self.emit_event(AgentLoopEvent::TestSuiteWarning {
            passed: outcome.passed,
            summary: outcome.summary,
            failed_tests: outcome.failed_tests,
        });
    }

    /// Resolve which test command the gate should run, returning the
    /// command string and a short label describing where it came from
    /// (rendered in the gate's status line so logs make the resolution
    /// transparent to the operator).
    ///
    /// Splitting this out from [`Self::check_all_tests_pass`] keeps the
    /// priority logic in one place and lets unit tests assert it without
    /// invoking the runner.
    pub(super) fn resolve_test_command(
        &self,
        project_root: &Path,
    ) -> Option<(String, &'static str)> {
        if let Some(cmd) = self
            .test_command_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some((cmd.to_string(), "env override"));
        }

        if let Some(cmd) = self
            .test_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some((cmd.to_string(), "project config"));
        }

        infer_default_test_command(project_root).map(|cmd| (cmd, "manifest auto-detect"))
    }

    async fn check_stubs_and_reject(&self) -> Option<String> {
        let mut attempts = self.stub_fix_attempts.lock().await;
        if *attempts >= MAX_STUB_FIX_ATTEMPTS {
            return None;
        }
        let base_path = Path::new(&self.project_folder);
        let ops = self.tracked_file_ops.lock().await;
        let stub_reports = file_ops::detect_stub_patterns(base_path, &ops);
        if stub_reports.is_empty() {
            return None;
        }
        *attempts += 1;
        let attempt = *attempts;

        self.emit_text(format!(
            "\n[stub detection] found {} stub(s), requesting fix (attempt {}/{})\n",
            stub_reports.len(),
            attempt,
            MAX_STUB_FIX_ATTEMPTS,
        ));

        Some(SteeringInjector::render(&SteeringKind::StubDetected {
            reports: stub_reports.clone(),
        }))
    }

    pub(super) async fn handle_submit_plan(
        &self,
        tc: &ToolCallInfo,
        results: &mut Vec<ToolCallResult>,
    ) {
        let plan = TaskPlan::from_tool_input(&tc.input);
        match plan.validate() {
            Ok(()) => {
                let context_string = plan.as_context_string();
                {
                    let mut phase = self.task_phase.lock().await;
                    *phase = TaskPhase::Implementing { plan };
                }
                // Clear the rolling outcome window on the exploration →
                // implementing transition. Exploration-phase errors
                // (directory-as-file reads, policy-denied commands, etc.)
                // should not vote against the implementation phase's
                // `task_done`; the guard only cares about recent
                // implementing-phase behaviour.
                {
                    let mut outcomes = self.recent_tool_outcomes.lock().await;
                    outcomes.reset();
                }
                // Tell the agent loop (if it shares this Arc) to reset
                // its exploration/read-guard counters on the next
                // iteration so the implement phase has a fresh budget.
                self.reset_explore_on_phase_change
                    .store(true, std::sync::atomic::Ordering::Release);
                results.push(Self::tool_result(
                    tc,
                    format!(
                        "Plan recorded for reference. Implementation can already \
                         proceed — writes (write_file/edit_file/delete_file) and \
                         task_done are accepted regardless of whether submit_plan \
                         was called. This call reset the rolling-outcome window.\n\n\
                         YOUR PLAN (reference during implementation):\n{context_string}\n\n\
                         Continue with the most foundational changes first.",
                    ),
                    false,
                    false,
                ));
            }
            Err(reason) => {
                results.push(Self::gate_rejection(
                    tc,
                    format!("Plan rejected: {reason}. Revise and resubmit."),
                ));
            }
        }
    }

    pub(super) fn handle_get_context(&self, tc: &ToolCallInfo, results: &mut Vec<ToolCallResult>) {
        results.push(Self::tool_result(
            tc,
            self.task_context.clone(),
            false,
            false,
        ));
    }

    pub(super) fn emit_tool_status(&self, tc: &ToolCallInfo, result: &ToolCallResult) {
        let arg_hint = format_tool_arg_hint(tc);
        let status_str = if result.is_error { "error" } else { "ok" };
        let marker = if arg_hint.is_empty() {
            format!("\n[tool: {} -> {}]\n", tc.name, status_str)
        } else {
            format!("\n[tool: {}({}) -> {}]\n", tc.name, arg_hint, status_str)
        };
        self.emit_text(marker);
    }

    /// Merge tracked executor state (file ops, notes, follow-ups) into a
    /// [`TaskExecutionResult`] so that downstream consumers see real evidence
    /// instead of hardcoded defaults.
    #[allow(clippy::assigning_clones)] // clone_from doesn't work through MutexGuard
    pub async fn merge_into_result(&self, exec: &mut crate::agent_runner::TaskExecutionResult) {
        exec.file_ops = self.tracked_file_ops.lock().await.clone();
        let task_notes = self.notes.lock().await.clone();
        if !task_notes.is_empty() {
            exec.notes = task_notes;
        }
        exec.follow_up_tasks = self.follow_ups.lock().await.clone();
        exec.no_changes_needed = *self.no_changes_needed.lock().await;
        let phase = self.task_phase.lock().await;
        exec.reached_implementing =
            matches!(*phase, crate::planning::TaskPhase::Implementing { .. });
    }

    pub(super) fn emit_text(&self, text: String) {
        self.emit_event(AgentLoopEvent::TextDelta(text));
    }

    pub(super) fn emit_event(&self, event: AgentLoopEvent) {
        if let Some(tx) = &self.event_tx {
            if let Err(e) = tx.try_send(event) {
                tracing::warn!("event channel full or closed: {e}");
            }
        }
    }
}

