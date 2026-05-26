//! Per-tick orchestration for [`super::DevLoopAutomaton`].
//!
//! Lifecycle:
//! - `on_install` emits a `LogLine` so operators can see the loop started.
//! - First `tick` initializes the queue: list tasks -> drop `done` ->
//!   sort by `order` -> store in `STATE_TASK_QUEUE`.
//! - Subsequent ticks pop one task, transition it to `in_progress`,
//!   execute through `AgentRunner::execute_task_tracked`, then record
//!   success or failure (transition + counter + event).
//! - `on_stop` emits `LoopFinished` if the loop did not already finish
//!   naturally.
//!
//! Codex parity (May 2026): the previous build retried `task_blocked`
//! / no-write failures once with a decomposition-prompt splice. With
//! the harness no longer manufacturing `task_blocked` envelopes (see
//! commits 1 and 2 in this series) the retry path is dead. The
//! automaton now classifies every executed task as success or failure
//! and stops on first failure, mirroring Codex's simple per-task
//! loop.

use std::sync::Arc;

use tracing::{info, warn};

use aura_agent::agent_runner::{AgenticTaskParams, TaskExecutionResult, TaskTrackingConfig};
use aura_agent::prompts::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};
use aura_agent::run_project_build_check;
use aura_tools::catalog::ToolProfile;
use aura_tools::domain_tools::TaskDescriptor;

use super::forward_event::spawn_agent_event_forwarder;
use super::{
    DevLoopAutomaton, DevLoopConfig, STATE_COMPLETED_COUNT, STATE_FAILED_COUNT, STATE_INITIALIZED,
    STATE_LOOP_FINISHED, STATE_TASK_QUEUE,
};
use crate::builtins::noop_executor::NoOpExecutor;
use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;
use crate::runtime::{Automaton, TickOutcome};
use crate::schedule::Schedule;

#[async_trait::async_trait]
impl Automaton for DevLoopAutomaton {
    fn kind(&self) -> &'static str {
        "dev-loop"
    }

    fn default_schedule(&self) -> Schedule {
        Schedule::Continuous
    }

    async fn on_install(&self, ctx: &TickContext) -> Result<(), AutomatonError> {
        let cfg = DevLoopConfig::from_json(&ctx.config)?;
        info!(project_id = %cfg.project_id, "Dev loop automaton installed");
        ctx.emit(AutomatonEvent::LogLine {
            message: format!("dev loop starting for project {}", cfg.project_id),
        })?;
        Ok(())
    }

    async fn tick(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        if ctx.is_cancelled() {
            return Ok(TickOutcome::Done);
        }

        let cfg = DevLoopConfig::from_json(&ctx.config)?;
        let initialized: bool = ctx.state.get(STATE_INITIALIZED).unwrap_or(false);

        if !initialized {
            return self.initialize_queue(ctx, &cfg).await;
        }

        self.process_next_task(ctx, &cfg).await
    }

    async fn on_stop(&self, ctx: &TickContext) -> Result<(), AutomatonError> {
        let already_finished: bool = ctx.state.get(STATE_LOOP_FINISHED).unwrap_or(false);
        if !already_finished {
            let completed: u32 = ctx.state.get(STATE_COMPLETED_COUNT).unwrap_or(0);
            let failed: u32 = ctx.state.get(STATE_FAILED_COUNT).unwrap_or(0);
            ctx.emit(AutomatonEvent::LoopFinished {
                outcome: "stopped".into(),
                completed_count: completed,
                failed_count: failed,
            })?;
        }
        Ok(())
    }
}

impl DevLoopAutomaton {
    async fn initialize_queue(
        &self,
        ctx: &mut TickContext,
        cfg: &DevLoopConfig,
    ) -> Result<TickOutcome, AutomatonError> {
        if self.tool_executor.is_none() {
            return Err(AutomatonError::InvalidConfig(
                "no tool executor configured â€” the agent cannot perform file or command operations"
                    .into(),
            ));
        }

        let mut tasks = self
            .domain
            .list_tasks(&cfg.project_id, None, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

        if tasks.is_empty() {
            info!("No tasks found for project, finishing");
            return self.finish(ctx);
        }

        tasks.retain(|t| t.status != "done");
        tasks.sort_by_key(|t| t.order);
        let queue: Vec<String> = tasks.into_iter().map(|t| t.id).collect();

        info!(remaining = queue.len(), "Task queue initialized");

        let pending = queue.len();
        ctx.state.set(STATE_TASK_QUEUE, &queue);
        ctx.state.set(STATE_INITIALIZED, &true);
        ctx.state.set(STATE_COMPLETED_COUNT, &0u32);
        ctx.state.set(STATE_FAILED_COUNT, &0u32);

        ctx.emit(AutomatonEvent::LogLine {
            message: format!("Dev loop ready: {pending} tasks to execute"),
        })?;

        Ok(TickOutcome::Continue)
    }

    async fn process_next_task(
        &self,
        ctx: &mut TickContext,
        cfg: &DevLoopConfig,
    ) -> Result<TickOutcome, AutomatonError> {
        let mut queue: Vec<String> = ctx.state.get(STATE_TASK_QUEUE).unwrap_or_default();

        if queue.is_empty() {
            info!("Task queue empty, finishing loop");
            return self.finish(ctx);
        }

        let task_id = queue.remove(0);
        ctx.state.set(STATE_TASK_QUEUE, &queue);

        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

        let task = match self.domain.get_task(&task_id, None).await {
            Ok(t) => t,
            Err(e) => {
                warn!(task_id = %task_id, error = %e, "Failed to fetch task, skipping");
                return Ok(TickOutcome::Continue);
            }
        };

        info!(task_id = %task.id, title = %task.title, "Starting task");

        if let Err(e) = self
            .domain
            .transition_task(&task.id, "in_progress", None)
            .await
        {
            warn!(task_id = %task.id, error = %e, "Failed to transition task to in_progress (continuing anyway)");
        }

        ctx.emit(AutomatonEvent::TaskStarted {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
        })?;

        let result = self
            .execute_task(ctx, cfg, &task, &project.build_command)
            .await;

        // User-initiated stop fires the shared cancellation token, which the
        // agent loop honours by returning an early `Ok(TaskExecutionResult)`
        // with empty `file_ops` and `no_changes_needed = false`. Without this
        // guard the empty result would trip `classify_execution_result`, mark
        // the task `failed`, increment `STATE_FAILED_COUNT`, and emit a
        // misleading `WARN Task execution failed ... task ended without writes
        // and without no_changes_needed`. Cancellation is not a failure --
        // log it cleanly and roll the task status back to `ready` so the
        // next dev loop start can pick it up.
        if ctx.is_cancelled() {
            return self
                .record_task_cancelled(ctx, &task)
                .await
                .map(|()| TickOutcome::Done);
        }

        match result {
            Ok(exec) => match classify_execution_result(&exec) {
                Some(err) => {
                    self.record_task_failure(ctx, &task, err).await?;
                    return self.finish_failed(ctx);
                }
                None => {
                    self.record_task_success(ctx, &task, exec).await?;
                }
            },
            Err(e) => {
                self.record_task_failure(ctx, &task, e).await?;
                return self.finish_failed(ctx);
            }
        }

        Ok(TickOutcome::Continue)
    }

    /// Mid-task cancellation handler.
    ///
    /// Called when the operator triggers a stop while a task is in flight.
    /// Distinguishes intentional cancellation from genuine failure: logs at
    /// `INFO`, leaves `STATE_FAILED_COUNT` untouched, transitions the task
    /// back to `ready`, and emits a `LogLine` event so the operator UI shows
    /// "Task <id> cancelled by stop request" instead of a phantom failure.
    async fn record_task_cancelled(
        &self,
        ctx: &mut TickContext,
        task: &TaskDescriptor,
    ) -> Result<(), AutomatonError> {
        info!(
            automaton_id = %ctx.automaton_id,
            task_id = %task.id,
            title = %task.title,
            "Task cancelled by user stop"
        );

        if let Err(e) = self.domain.transition_task(&task.id, "ready", None).await {
            warn!(
                task_id = %task.id,
                error = %e,
                "Failed to roll cancelled task back to ready"
            );
        }

        ctx.emit(AutomatonEvent::LogLine {
            message: format!("Task {} cancelled by stop request", task.id),
        })?;
        Ok(())
    }

    async fn record_task_success(
        &self,
        ctx: &mut TickContext,
        task: &TaskDescriptor,
        exec: TaskExecutionResult,
    ) -> Result<(), AutomatonError> {
        if let Err(e) = self.domain.transition_task(&task.id, "done", None).await {
            warn!(task_id = %task.id, error = %e, "Failed to sync task done status to backend");
        }

        let completed: u32 = ctx.state.get(STATE_COMPLETED_COUNT).unwrap_or(0) + 1;
        ctx.state.set(STATE_COMPLETED_COUNT, &completed);

        ctx.emit(AutomatonEvent::TaskCompleted {
            task_id: task.id.clone(),
            summary: exec.notes,
        })?;
        ctx.emit(AutomatonEvent::TokenUsage {
            task_id: Some(task.id.clone()),
            input_tokens: exec.input_tokens,
            output_tokens: exec.output_tokens,
        })?;

        info!(task_id = %task.id, title = %task.title, "Task completed successfully");
        Ok(())
    }

    async fn record_task_failure(
        &self,
        ctx: &mut TickContext,
        task: &TaskDescriptor,
        e: AutomatonError,
    ) -> Result<(), AutomatonError> {
        warn!(task_id = %task.id, error = %e, "Task execution failed");

        if let Err(te) = self.domain.transition_task(&task.id, "failed", None).await {
            warn!(task_id = %task.id, error = %te, "Failed to sync task failed status to backend");
        }

        let failed: u32 = ctx.state.get(STATE_FAILED_COUNT).unwrap_or(0) + 1;
        ctx.state.set(STATE_FAILED_COUNT, &failed);

        ctx.emit(AutomatonEvent::TaskFailed {
            task_id: task.id.clone(),
            reason: e.to_string(),
        })?;
        Ok(())
    }

    async fn execute_task(
        &self,
        ctx: &TickContext,
        cfg: &DevLoopConfig,
        task: &TaskDescriptor,
        build_command: &Option<String>,
    ) -> Result<TaskExecutionResult, AutomatonError> {
        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;
        let spec = self
            .domain
            .get_spec(&task.spec_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

        let effective_path = ctx
            .workspace_root
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| project.path.clone());

        let project_info = ProjectInfo {
            project_id: None,
            name: &project.name,
            description: project.description.as_deref().unwrap_or(""),
            folder_path: &effective_path,
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
            execution_notes: "",
            files_changed: &[],
        };
        let session_info = SessionInfo {
            summary_of_previous_context: "",
        };

        let tools = self.catalog.tools_for_profile(ToolProfile::Engine);

        // Borrow the parsed identity envelope (if any) as a transient
        // `AgentInfo<'_>` so `SystemPromptBuilder` renders the
        // `<agent_identity>` / `<agent_skills>` / `<agent_system_prompt>`
        // sections. `as_agent_info()` returns `None` whenever the
        // wire fields are absent / blank, leaving the prompt
        // byte-identical to the empty-identity baseline.
        let agent_info = cfg.agent_identity.as_agent_info();

        let params = AgenticTaskParams {
            project: &project_info,
            spec: &spec_info,
            task: &task_info,
            session: &session_info,
            work_log: &[],
            completed_deps: &[],
            workspace_map: "",
            codebase_snapshot: "",
            type_defs_context: "",
            dep_api_context: "",
            member_count: 1,
            tools,
            attempt: 0,
            agent: agent_info.as_ref(),
        };

        // Inner channel: the agent loop emits advisory events
        // (`TextDelta` / `ThinkingDelta` / `ToolStart` /
        // `ToolInputSnapshot` / `ToolCallCompleted` / `ToolResult`)
        // here at a high cadence on the E.4 streaming-pump path. The
        // forwarder consumes them and projects through
        // `forward_agent_event` onto `ctx.event_tx`. See
        // `forward_event.rs` for the post-E.4 drop policy that keeps
        // this from flooding the operator log when the outer consumer
        // is briefly behind or has already torn down.
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);
        let _forwarder =
            spawn_agent_event_forwarder(ctx.event_tx.clone(), event_rx, Some(task.id.clone()));

        let inner_executor: Arc<dyn aura_agent::types::AgentToolExecutor> = self
            .tool_executor
            .clone()
            .unwrap_or_else(|| Arc::new(NoOpExecutor));

        let tracking = TaskTrackingConfig {
            inner_executor,
            project_folder: effective_path.clone(),
            build_command: project.build_command.clone(),
            test_command: project.test_command.clone(),
        };

        let cancel = ctx.cancellation_token().clone();
        let exec = self
            .runner
            .execute_task_tracked(
                self.provider.as_ref(),
                tracking,
                &params,
                Some(event_tx),
                Some(cancel),
            )
            .await
            .map_err(|e| AutomatonError::AgentExecution(e.to_string()))?;

        if !ctx.is_cancelled() {
            verify_build_after_agent(&effective_path, build_command.as_deref()).await?;
        }
        Ok(exec)
    }

    fn finish(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        Self::finish_with_outcome(ctx, LoopFinishOutcome::Completed)
    }

    fn finish_failed(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        Self::finish_with_outcome(ctx, LoopFinishOutcome::Failed)
    }

    fn finish_with_outcome(
        ctx: &mut TickContext,
        outcome: LoopFinishOutcome,
    ) -> Result<TickOutcome, AutomatonError> {
        let completed: u32 = ctx.state.get(STATE_COMPLETED_COUNT).unwrap_or(0);
        let failed: u32 = ctx.state.get(STATE_FAILED_COUNT).unwrap_or(0);
        ctx.state.set(STATE_LOOP_FINISHED, &true);
        ctx.emit(AutomatonEvent::LoopFinished {
            outcome: outcome.as_str().into(),
            completed_count: completed,
            failed_count: failed,
        })?;
        Ok(TickOutcome::Done)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopFinishOutcome {
    Completed,
    Failed,
}

impl LoopFinishOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Layer C (Issue A defense-in-depth): classify an
/// `Ok(TaskExecutionResult)` from
/// [`aura_agent::agent_runner::AgentRunner::execute_task_tracked`]
/// into "success" (returns `None`) or "treat as failure" (returns
/// `Some(AutomatonError)`).
///
/// Refusing empty results here (no `file_ops` AND no explicit
/// `no_changes_needed` flag) keeps the automaton honest regardless
/// of what the loop did upstream.
///
/// `no_changes_needed = true` is the legitimate no-op completion
/// (the agent inspected the codebase and concluded that the task
/// description was satisfied by existing code); that branch returns
/// `None` so the task is recorded as success.
async fn verify_build_after_agent(
    project_folder: &str,
    build_command: Option<&str>,
) -> Result<(), AutomatonError> {
    run_project_build_check(project_folder, build_command)
        .await
        .map_err(AutomatonError::AgentExecution)
}

fn classify_execution_result(exec: &TaskExecutionResult) -> Option<AutomatonError> {
    if exec.file_ops.is_empty() && !exec.no_changes_needed {
        Some(AutomatonError::AgentExecution(
            "task ended without writes and without no_changes_needed".into(),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn empty_exec() -> TaskExecutionResult {
        TaskExecutionResult::default()
    }

    fn test_context() -> (TickContext, tokio::sync::mpsc::Receiver<AutomatonEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let ctx = TickContext::new(
            crate::types::AutomatonId::from_string("test-dev-loop"),
            crate::state::AutomatonState::new(),
            tx,
            serde_json::json!({}),
            None,
            tokio_util::sync::CancellationToken::new(),
        );
        (ctx, rx)
    }

    /// Layer C contract: an empty [`TaskExecutionResult`] (no
    /// `file_ops` AND `no_changes_needed = false`) must be classified
    /// as a failure so the dev-loop automaton records it via
    /// [`super::Automaton::record_task_failure`] (incrementing
    /// `STATE_FAILED_COUNT` and emitting `TaskFailed`) instead of
    /// silently marking the task as `done`.
    #[test]
    fn empty_file_ops_and_no_no_changes_flag_classifies_as_failure() {
        let exec = empty_exec();
        let err = classify_execution_result(&exec)
            .expect("empty TaskExecutionResult must classify as failure");
        // The dev-loop automaton's `record_task_failure` surfaces the
        // error string in the operator UI verbatim, so guarding it
        // here pins the wire contract.
        let msg = err.to_string();
        assert!(
            msg.contains("ended without writes") && msg.contains("no_changes_needed"),
            "the failure message must point at the empty-completion failure mode; got {msg:?}",
        );
    }

    /// `no_changes_needed: true` is the legitimate no-op completion
    /// path — the agent inspected the codebase and concluded the task
    /// description was satisfied by existing code. Even with an empty
    /// `file_ops` list, this must record as success so the loop
    /// doesn't retry the task forever.
    #[test]
    fn no_changes_needed_flag_classifies_as_success() {
        let mut exec = empty_exec();
        exec.no_changes_needed = true;
        assert!(
            classify_execution_result(&exec).is_none(),
            "`no_changes_needed: true` is the legitimate no-op completion path \
             (task description satisfied by existing code) and must record as success",
        );
    }

    #[test]
    fn terminal_task_failure_finishes_loop_as_failed() {
        let (mut ctx, mut rx) = test_context();
        ctx.state.set(STATE_COMPLETED_COUNT, &2u32);
        ctx.state.set(STATE_FAILED_COUNT, &1u32);

        let outcome = DevLoopAutomaton::finish_with_outcome(&mut ctx, LoopFinishOutcome::Failed)
            .expect("failed finish should emit LoopFinished");

        assert!(matches!(outcome, TickOutcome::Done));
        assert!(
            ctx.state.get::<bool>(STATE_LOOP_FINISHED).unwrap_or(false),
            "failed finish must suppress on_stop's secondary LoopFinished event"
        );
        match rx.try_recv().expect("LoopFinished event expected") {
            AutomatonEvent::LoopFinished {
                outcome,
                completed_count,
                failed_count,
            } => {
                assert_eq!(outcome, "failed");
                assert_eq!(completed_count, 2);
                assert_eq!(failed_count, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // NOTE: a third case — non-empty `file_ops` classifies as success —
    // is intentionally omitted here because `aura_agent::file_ops::FileOp`
    // is `pub(crate)` within `aura-agent` and cannot be named from the
    // `aura-automaton` test module. The empty-vs-non-empty branch is
    // exercised end-to-end by the agent-loop / agent-runner test
    // suites which DO have access to construct `FileOp` values; here
    // we cover only the new defense-in-depth empty-empty rejection.
}
