//! Per-tick orchestration for [`super::DevLoopAutomaton`].
//!
//! Lifecycle:
//! - `on_install` parses the start-request JSON exactly once (see
//!   [`super::DevLoopAutomaton::parsed_config`]) and emits a `LogLine`
//!   so operators can see the loop started. Subsequent ticks read the
//!   stashed [`DevLoopConfig`] instead of reparsing JSON.
//! - First `tick` initializes the queue: list tasks -> drop `done` ->
//!   sort by `order` -> stash via the typed
//!   [`DevLoopState`] blob.
//! - Subsequent ticks pop one task, transition it to `in_progress`,
//!   hand off to [`super::super::common::run_tracked_task`], then
//!   record success or failure through
//!   [`super::super::common::finalize_task_outcome`].
//! - `on_stop` emits `LoopFinished` if the loop did not already finish
//!   naturally.
//!
//! Retry policy: each task is run once, then a project-level build
//! check (`verify_build_after_agent`) is performed. If the build comes
//! back red, the task is re-run **once** with the truncated build
//! output spliced into the description via `build_retry_note` (see
//! [`Self::execute_task_with_build_retry`]). A second red build is
//! treated as a hard failure and the loop transitions the task to
//! `failed` before halting on first failure — mirroring Codex's
//! simple per-task loop semantics. The earlier `task_blocked` retry
//! envelope (May 2026 codex-parity sweep) is gone; only the
//! build-output retry described here remains.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use aura_agent::run_project_build_check;
use aura_tools::domain_tools::TaskDescriptor;

use super::DevLoopAutomaton;
use crate::builtins::common::config::SharedDevLoopConfig;
use crate::builtins::common::{
    finalize_task_outcome, run_tracked_task, DevLoopConfig, TaskExecutionRequest, TaskOutcome,
};
use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;
use crate::runtime::{Automaton, TickOutcome};
use crate::schedule::Schedule;

/// Single blob holding all per-tick scratch state. Replaces the
/// pre-Phase-6 set of magic-string `AutomatonState` keys
/// (`"initialized"` / `"task_queue"` / `"completed_count"` /
/// `"failed_count"` / `"loop_finished"`). One struct, one source of
/// truth, one serde error path. The compiler now enforces the field
/// set instead of relying on every reader to spell the same key
/// string the writer used.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct DevLoopState {
    initialized: bool,
    task_queue: Vec<String>,
    completed_count: u32,
    failed_count: u32,
    loop_finished: bool,
}

/// Magic-string key the typed [`DevLoopState`] is serialized under.
/// The single private constant exists so a future rename only has to
/// happen here; nothing outside this module references it.
const DEV_LOOP_STATE_KEY: &str = "dev_loop_state";

impl DevLoopState {
    fn load(ctx: &TickContext) -> Self {
        ctx.state.get(DEV_LOOP_STATE_KEY).unwrap_or_default()
    }

    fn save(&self, ctx: &mut TickContext) -> Result<(), AutomatonError> {
        ctx.state.set(DEV_LOOP_STATE_KEY, self)
    }
}

#[async_trait::async_trait]
impl Automaton for DevLoopAutomaton {
    fn kind(&self) -> &'static str {
        "dev-loop"
    }

    fn default_schedule(&self) -> Schedule {
        Schedule::Continuous
    }

    async fn on_install(&self, ctx: &TickContext) -> Result<(), AutomatonError> {
        let cfg = self.parse_or_get_config(ctx)?;
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

        let cfg = self.parse_or_get_config(ctx)?;
        let mut state = DevLoopState::load(ctx);

        if !state.initialized {
            return self.initialize_queue(ctx, &cfg, &mut state).await;
        }

        self.process_next_task(ctx, &cfg, &mut state).await
    }

    async fn on_stop(&self, ctx: &TickContext) -> Result<(), AutomatonError> {
        let state = DevLoopState::load(ctx);
        if !state.loop_finished {
            ctx.emit(AutomatonEvent::LoopFinished {
                outcome: "stopped".into(),
                completed_count: state.completed_count,
                failed_count: state.failed_count,
            })?;
        }
        Ok(())
    }
}

impl DevLoopAutomaton {
    /// Parse the start-request JSON exactly once and stash the typed
    /// `DevLoopConfig` on the automaton struct. Subsequent calls
    /// return the same `Arc` without re-touching the JSON. Errors on
    /// the first parse (missing model, missing project_id) propagate
    /// out of `on_install`; subsequent ticks observe the
    /// successfully-stashed config.
    fn parse_or_get_config(
        &self,
        ctx: &TickContext,
    ) -> Result<SharedDevLoopConfig, AutomatonError> {
        if let Some(cfg) = self.parsed_config.get() {
            return Ok(cfg.clone());
        }
        let parsed = DevLoopConfig::from_json(&ctx.config)?;
        let cfg: SharedDevLoopConfig = Arc::new(parsed);
        // `OnceLock::set` only returns Err if another thread won the
        // race; if so, just use the value the winner installed —
        // both are derived from the same JSON.
        let _ = self.parsed_config.set(cfg.clone());
        Ok(self.parsed_config.get().cloned().unwrap_or(cfg))
    }

    async fn initialize_queue(
        &self,
        ctx: &mut TickContext,
        cfg: &DevLoopConfig,
        state: &mut DevLoopState,
    ) -> Result<TickOutcome, AutomatonError> {
        if self.tool_executor.is_none() {
            return Err(AutomatonError::InvalidConfig(
                "no tool executor configured — the agent cannot perform file or command operations"
                    .into(),
            ));
        }

        let mut tasks = self
            .domain
            .list_tasks(&cfg.project_id, None, None)
            .await
            .map_err(|e| AutomatonError::domain_api(None, e))?;

        if tasks.is_empty() {
            info!("No tasks found for project, finishing");
            return self.finish(ctx, state);
        }

        tasks.retain(|t| t.status != "done");
        tasks.sort_by_key(|t| t.order);
        let queue: Vec<String> = tasks.into_iter().map(|t| t.id).collect();

        info!(remaining = queue.len(), "Task queue initialized");

        let pending = queue.len();
        state.task_queue = queue;
        state.initialized = true;
        state.completed_count = 0;
        state.failed_count = 0;
        state.save(ctx)?;

        ctx.emit(AutomatonEvent::LogLine {
            message: format!("Dev loop ready: {pending} tasks to execute"),
        })?;

        Ok(TickOutcome::Continue)
    }

    async fn process_next_task(
        &self,
        ctx: &mut TickContext,
        cfg: &DevLoopConfig,
        state: &mut DevLoopState,
    ) -> Result<TickOutcome, AutomatonError> {
        if state.task_queue.is_empty() {
            info!("Task queue empty, finishing loop");
            return self.finish(ctx, state);
        }

        let task_id = state.task_queue.remove(0);
        state.save(ctx)?;

        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(Some(task_id.clone()), e))?;

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
            .execute_task_with_build_retry(ctx, cfg, &project, &task)
            .await;

        let outcome = finalize_task_outcome(ctx, self.domain.as_ref(), &task, result).await?;

        match outcome {
            TaskOutcome::Cancelled => {
                // User-initiated stop — the finalizer rolled the task
                // back to `ready` and emitted the `LogLine`. Leave
                // counters untouched and exit the loop cleanly.
                Ok(TickOutcome::Done)
            }
            TaskOutcome::Success => {
                state.completed_count = state.completed_count.saturating_add(1);
                state.save(ctx)?;
                Ok(TickOutcome::Continue)
            }
            TaskOutcome::Failure => {
                state.failed_count = state.failed_count.saturating_add(1);
                state.save(ctx)?;
                self.finish_failed(ctx, state)
            }
        }
    }

    /// Run the agent once; if the tree is still red, retry once with stderr.
    async fn execute_task_with_build_retry(
        &self,
        ctx: &TickContext,
        cfg: &DevLoopConfig,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        task: &TaskDescriptor,
    ) -> Result<aura_agent::agent_runner::TaskExecutionResult, AutomatonError> {
        let effective_path = ctx
            .workspace_root_str()
            .unwrap_or_else(|| project.path.clone());
        let exec = self.execute_task(ctx, cfg, project, task, None).await?;
        if ctx.is_cancelled() {
            return Ok(exec);
        }
        let Err(build_err) =
            verify_build_after_agent(&effective_path, project.build_command.as_deref()).await
        else {
            return Ok(exec);
        };
        info!(
            task_id = %task.id,
            "Build still failing after agent pass; retrying once with compiler output"
        );
        let retry_note = truncate_for_retry(
            &build_err.to_string(),
            aura_config::DEV_LOOP_RETRY_NOTE_MAX_BYTES,
        );
        let exec = self
            .execute_task(ctx, cfg, project, task, Some(retry_note))
            .await?;
        if ctx.is_cancelled() {
            return Ok(exec);
        }
        verify_build_after_agent(&effective_path, project.build_command.as_deref()).await?;
        Ok(exec)
    }

    async fn execute_task(
        &self,
        ctx: &TickContext,
        cfg: &DevLoopConfig,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        task: &TaskDescriptor,
        build_retry_note: Option<String>,
    ) -> Result<aura_agent::agent_runner::TaskExecutionResult, AutomatonError> {
        let spec = self
            .domain
            .get_spec(&task.spec_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(Some(task.id.clone()), e))?;

        // Pre-implementation refinement. Only run on the first pass
        // (`build_retry_note.is_none()`); the build-retry second
        // pass would otherwise re-refine the description that
        // already had the compiler-output appended below, doubling
        // up the marker and confusing the agent. The helper carries
        // its own idempotency marker (`<!-- aura-refined:v1 -->`)
        // as a second safety net for ambient re-claims.
        let task_owned = if build_retry_note.is_none() {
            crate::builtins::task_refinement::refine_task_description(
                self.domain.as_ref(),
                self.provider.as_ref(),
                &cfg.model,
                &spec,
                task,
                Some(ctx.event_sender()),
            )
            .await?
        } else {
            task.clone()
        };

        run_tracked_task(TaskExecutionRequest {
            ctx,
            runner: &self.runner,
            provider: self.provider.as_ref(),
            catalog: self.catalog.as_ref(),
            task: &task_owned,
            spec: &spec,
            project,
            identity: &cfg.agent_identity,
            tool_executor: self.tool_executor.clone(),
            // Phase 5: dev-loop tasks default to runner-level
            // `early_test_oracle` (set `true` for task-shaped
            // automaton runners via
            // `AgentRunnerConfig.early_test_oracle`). Leaving `None`
            // here means "use the runner default" rather than
            // forcing the per-task override.
            early_test_oracle: None,
            build_retry_note,
        })
        .await
    }

    fn finish(
        &self,
        ctx: &mut TickContext,
        state: &mut DevLoopState,
    ) -> Result<TickOutcome, AutomatonError> {
        Self::finish_with_outcome(ctx, state, LoopFinishOutcome::Completed)
    }

    fn finish_failed(
        &self,
        ctx: &mut TickContext,
        state: &mut DevLoopState,
    ) -> Result<TickOutcome, AutomatonError> {
        Self::finish_with_outcome(ctx, state, LoopFinishOutcome::Failed)
    }

    fn finish_with_outcome(
        ctx: &mut TickContext,
        state: &mut DevLoopState,
        outcome: LoopFinishOutcome,
    ) -> Result<TickOutcome, AutomatonError> {
        state.loop_finished = true;
        state.save(ctx)?;
        ctx.emit(AutomatonEvent::LoopFinished {
            outcome: outcome.as_str().into(),
            completed_count: state.completed_count,
            failed_count: state.failed_count,
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

async fn verify_build_after_agent(
    project_folder: &str,
    build_command: Option<&str>,
) -> Result<(), AutomatonError> {
    run_project_build_check(project_folder, build_command)
        .await
        .map_err(|msg| {
            AutomatonError::agent_execution(None, aura_agent::AgentError::BuildFailed(msg))
        })
}

fn truncate_for_retry(message: &str, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message.to_string();
    }
    let half = max_bytes / 2;
    let start = &message[..floor_char_boundary(message, half)];
    let end = &message[ceil_char_boundary(message, message.len() - half)..];
    format!("{start}\n\n... (truncated) ...\n\n{end}")
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

#[cfg(test)]
mod completion_tests {
    use super::*;

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

    #[test]
    fn terminal_task_failure_finishes_loop_as_failed() {
        let (mut ctx, mut rx) = test_context();
        let mut state = DevLoopState {
            completed_count: 2,
            failed_count: 1,
            ..DevLoopState::default()
        };

        let outcome =
            DevLoopAutomaton::finish_with_outcome(&mut ctx, &mut state, LoopFinishOutcome::Failed)
                .expect("failed finish should emit LoopFinished");

        assert!(matches!(outcome, TickOutcome::Done));
        let stored = DevLoopState::load(&ctx);
        assert!(
            stored.loop_finished,
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

    #[test]
    fn state_serializes_as_single_blob() {
        // Regression for the magic-string-key bag: writing the typed
        // `DevLoopState` once must round-trip every field through
        // `AutomatonState::get/set` (one JSON blob, one key).
        let mut state = AutomatonState::new();
        let saved = DevLoopState {
            initialized: true,
            task_queue: vec!["a".into(), "b".into()],
            completed_count: 3,
            failed_count: 1,
            loop_finished: false,
        };
        state.set(DEV_LOOP_STATE_KEY, &saved).expect("serialize");
        let loaded: DevLoopState = state.get(DEV_LOOP_STATE_KEY).expect("deserialize");
        assert!(loaded.initialized);
        assert_eq!(loaded.task_queue, vec!["a", "b"]);
        assert_eq!(loaded.completed_count, 3);
        assert_eq!(loaded.failed_count, 1);
        assert!(!loaded.loop_finished);
        // Exactly one key written — the one we wrote.
        let keys: Vec<&String> = state.keys().collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], DEV_LOOP_STATE_KEY);
    }

    use crate::state::AutomatonState;
}
