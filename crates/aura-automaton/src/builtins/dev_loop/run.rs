use super::safe_transition::{safe_transition, TransitionOutcome};
use super::{
    debug, extract_shell_command, forward_agent_event, info, validate_execution, warn,
    AgenticTaskParams, Arc, AutomatonError, AutomatonEvent, DevLoopAutomaton, DevLoopConfig,
    DomainApi, HashMap, ProjectInfo, SessionInfo, ShellTaskParams, SpecInfo, TaskDescriptor,
    TaskExecutionResult, TaskInfo, TaskTrackingConfig, TickContext, ToolProfile,
    MAX_RETRIES_PER_TASK, STATE_FAILED_IDS, STATE_FAILURE_REASONS, STATE_RETRY_COUNTS,
    STATE_TASK_QUEUE, STATE_WORK_LOG,
};
use crate::builtins::noop_executor::NoOpExecutor;

impl DevLoopAutomaton {
    pub(super) async fn execute_task(
        &self,
        ctx: &TickContext,
        cfg: &DevLoopConfig,
        task: &TaskDescriptor,
    ) -> Result<TaskExecutionResult, AutomatonError> {
        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

        if let Some(shell_cmd) = extract_shell_command(task) {
            return self.execute_shell(ctx, &project, &shell_cmd).await;
        }

        let effective_path = effective_project_path(ctx, &project);
        let spec = self
            .domain
            .get_spec(&task.spec_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

        let exec = self
            .run_agentic_task(ctx, cfg, &project, &spec, task, &effective_path)
            .await?;
        validate_execution(exec)
    }

    async fn execute_shell(
        &self,
        ctx: &TickContext,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        shell_cmd: &str,
    ) -> Result<TaskExecutionResult, AutomatonError> {
        let workspace = ctx
            .workspace_root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&project.path));
        self.runner
            .execute_shell_task(
                &ShellTaskParams {
                    command: shell_cmd,
                    project_root: workspace,
                },
                None,
            )
            .await
            .map_err(|e| AutomatonError::AgentExecution(e.to_string()))
    }

    async fn run_agentic_task(
        &self,
        ctx: &TickContext,
        cfg: &DevLoopConfig,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        spec: &aura_tools::domain_tools::SpecDescriptor,
        task: &TaskDescriptor,
        effective_path: &str,
    ) -> Result<TaskExecutionResult, AutomatonError> {
        let failure_reasons: HashMap<String, String> =
            ctx.state.get(STATE_FAILURE_REASONS).unwrap_or_default();
        let prior_failure = failure_reasons.get(&task.id).cloned().unwrap_or_default();
        let work_log: Vec<String> = ctx.state.get(STATE_WORK_LOG).unwrap_or_default();
        // Phase 4: the dev-loop's per-task retry counter is the source
        // of truth for `attempt`. 0 on the first run; 1, 2, ... after
        // each `try_retry_failed` increment. The counter is the same
        // one the dev-loop already uses to gate retries against
        // `MAX_RETRIES_PER_TASK`, so we don't introduce a parallel
        // notion of "how many times has this task been tried".
        let retry_counts: HashMap<String, u32> =
            ctx.state.get(STATE_RETRY_COUNTS).unwrap_or_default();
        let attempt = retry_counts.get(&task.id).copied().unwrap_or(0);
        // Dev-loop tool surface (Phase E of harness-v2.2): swap the
        // granular `write_file` / `edit_file` / `delete_file` for the
        // unified `apply_patch` primitive. The granular tools stay
        // registered in the Engine profile so chat-mode and other
        // callers keep them, but inside the dev-loop we hide them so
        // the model has exactly ONE write entry point. This kills the
        // ambiguity that powered the read-only loop trap and lets
        // Phase B's `had_any_file_write` latch trip cleanly on a
        // single successful `apply_patch`.
        let tools: Vec<_> = self
            .catalog
            .tools_for_profile(ToolProfile::Engine)
            .into_iter()
            .filter(|t| !matches!(t.name.as_str(), "write_file" | "edit_file" | "delete_file"))
            .collect();

        let project_info = ProjectInfo {
            name: &project.name,
            description: project.description.as_deref().unwrap_or(""),
            folder_path: effective_path,
            build_command: project.build_command.as_deref(),
            test_command: project.test_command.as_deref(),
        };
        let spec_info = SpecInfo {
            title: &spec.title,
            markdown_contents: &spec.content,
        };
        let task_info = TaskInfo {
            title: &task.title,
            description: &task.description,
            execution_notes: &prior_failure,
            files_changed: &[],
        };
        let session_info = SessionInfo {
            summary_of_previous_context: "",
        };

        // PR B: re-added wire field. The dev-loop start handler
        // does not yet populate `agent_identity` / `agent_skills` /
        // `agent_system_prompt` on the aura-os side, so
        // `as_agent_info()` returns `None` for every production caller
        // in this PR and the assembled system prompt stays
        // byte-identical with PR A. PR C flips the populator and
        // identity flows through automatically.
        let agent_info = cfg.agent_identity.as_agent_info();
        let params = AgenticTaskParams {
            project: &project_info,
            spec: &spec_info,
            task: &task_info,
            session: &session_info,
            work_log: &work_log,
            completed_deps: &[],
            workspace_map: "",
            codebase_snapshot: "",
            type_defs_context: "",
            dep_api_context: "",
            member_count: 1,
            tools,
            attempt,
            agent: agent_info.as_ref(),
        };

        let cancel = ctx.cancellation_token().clone();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1024);
        let automaton_tx = ctx.event_tx.clone();
        let task_id = task.id.clone();
        tokio::spawn(async move {
            while let Some(evt) = event_rx.recv().await {
                forward_agent_event(&automaton_tx, evt, Some(&task_id));
            }
        });

        let inner_executor: Arc<dyn aura_agent::types::AgentToolExecutor> = self
            .tool_executor
            .clone()
            .unwrap_or_else(|| Arc::new(NoOpExecutor));

        let tracking = TaskTrackingConfig {
            inner_executor,
            project_folder: effective_path.to_string(),
            build_command: project.build_command.clone(),
            test_command: project.test_command.clone(),
        };

        self.runner
            .execute_task_tracked(
                self.provider.as_ref(),
                tracking,
                &params,
                Some(event_tx),
                Some(cancel),
            )
            .await
            .map_err(|e| AutomatonError::AgentExecution(e.to_string()))
    }

    pub(super) async fn try_retry_failed(
        &self,
        ctx: &mut TickContext,
        _project_id: &str,
    ) -> Result<bool, AutomatonError> {
        let failed_ids: Vec<String> = ctx.state.get(STATE_FAILED_IDS).unwrap_or_default();
        if failed_ids.is_empty() {
            return Ok(false);
        }

        let mut retry_counts: HashMap<String, u32> =
            ctx.state.get(STATE_RETRY_COUNTS).unwrap_or_default();

        let retryable: Vec<String> = failed_ids
            .iter()
            .filter(|id| *retry_counts.get(*id).unwrap_or(&0) < MAX_RETRIES_PER_TASK)
            .cloned()
            .collect();

        if retryable.is_empty() {
            return Ok(false);
        }

        enqueue_retries(
            ctx,
            self.domain.as_ref(),
            &retryable,
            &mut retry_counts,
            &failed_ids,
        )
        .await?;

        Ok(true)
    }
}

async fn enqueue_retries(
    ctx: &mut TickContext,
    domain: &dyn DomainApi,
    retryable: &[String],
    retry_counts: &mut HashMap<String, u32>,
    failed_ids: &[String],
) -> Result<(), AutomatonError> {
    let mut queue: Vec<String> = ctx.state.get(STATE_TASK_QUEUE).unwrap_or_default();
    let new_failed: Vec<String> = failed_ids
        .iter()
        .filter(|id| !retryable.contains(id))
        .cloned()
        .collect();

    for id in retryable {
        let count = retry_counts.entry(id.clone()).or_insert(0);
        *count += 1;
        info!(task_id = %id, attempt = *count, "Retrying failed task");

        match safe_transition(domain, id, "ready").await {
            Ok(TransitionOutcome::Applied) => {}
            Ok(TransitionOutcome::AlreadyInTarget) => {
                debug!(task_id = %id, status = "ready", "Task already in target state; skipping retry sync");
            }
            Ok(TransitionOutcome::LocalOnlyMissing) => {
                debug!(task_id = %id, status = "ready", "Task not on backend (404); skipping retry sync");
            }
            Err(e) => {
                warn!(task_id = %id, error = %e, "Failed to sync retry status to backend");
            }
        }

        queue.push(id.clone());
        ctx.emit(AutomatonEvent::TaskRetrying {
            task_id: id.clone(),
            attempt: *count,
            reason: "automatic retry after failure".into(),
        })?;
    }

    ctx.state.set(STATE_TASK_QUEUE, &queue);
    ctx.state.set(STATE_FAILED_IDS, &new_failed);
    ctx.state.set(STATE_RETRY_COUNTS, retry_counts);
    Ok(())
}

fn effective_project_path(
    ctx: &TickContext,
    project: &aura_tools::domain_tools::ProjectDescriptor,
) -> String {
    ctx.workspace_root
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| project.path.clone())
}
