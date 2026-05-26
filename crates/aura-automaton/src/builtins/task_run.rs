//! Single-task runner automaton.
//!
//! Replaces `DevLoopEngine::run_single_task()` from `aura-app`. On-demand:
//! a single tick executes one task and returns `Done`.
//!
//! Stripped (2026-05) of the same guards the dev-loop simplification
//! removed: no `safe_transition` bridging (direct `transition_task`),
//! no `TaskAggregate` / `commit_and_push` (no end-of-task git), no
//! `validate_execution` wrapper, no `extract_shell_command` shortcut
//! for `run:` / `shell:` titled tasks, no `prior_failure` / `work_log`
//! retry warm-up plumbing. PR C re-introduces `AgentIdentityEnvelope`.

use std::sync::Arc;

use tracing::{error, info, warn};

use aura_agent::agent_runner::{
    AgentRunner, AgentRunnerConfig, AgenticTaskParams, TaskTrackingConfig,
};
use aura_agent::prompts::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};
use aura_reasoner::ModelProvider;
use aura_tools::catalog::{ToolCatalog, ToolProfile};
use aura_tools::domain_tools::DomainApi;

use super::noop_executor::NoOpExecutor;
use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;
use crate::runtime::{Automaton, TickOutcome};
use crate::schedule::Schedule;

pub struct TaskRunAutomaton {
    domain: Arc<dyn DomainApi>,
    provider: Arc<dyn ModelProvider>,
    runner: AgentRunner,
    catalog: Arc<ToolCatalog>,
    tool_executor: Option<Arc<dyn aura_agent::types::AgentToolExecutor>>,
}

impl TaskRunAutomaton {
    /// Construct a task-run automaton bound to a kernel-mediated model
    /// provider.
    pub fn new<P>(
        domain: Arc<dyn DomainApi>,
        provider: Arc<P>,
        config: AgentRunnerConfig,
        catalog: Arc<ToolCatalog>,
    ) -> Self
    where
        P: aura_agent::RecordingModelProvider + Send + Sync + 'static,
    {
        let provider: Arc<dyn ModelProvider> = provider;
        Self {
            domain,
            provider,
            runner: AgentRunner::new(config),
            catalog,
            tool_executor: None,
        }
    }

    #[must_use]
    pub fn with_tool_executor(
        mut self,
        executor: Arc<dyn aura_agent::types::AgentToolExecutor>,
    ) -> Self {
        self.tool_executor = Some(executor);
        self
    }
}

#[derive(Debug)]
struct TaskRunConfig {
    project_id: String,
    task_id: String,
    #[allow(dead_code)]
    agent_instance_id: String,
    /// Identity envelope parsed off the dispatch JSON. Reuses the
    /// `AgentIdentityEnvelope` from `dev_loop` so the two automatons
    /// share a single parser + render pathway. Stays empty
    /// (`is_empty == true`) until the aura-os populator lands.
    agent_identity: super::dev_loop::AgentIdentityEnvelope,
    /// Phase 3a (reread-efficiency plan): opt-in switch for the early
    /// "is the test gate already green?" oracle. The state machine
    /// + steering kind live in
    /// `aura_agent::prompts::steering::EarlyTestOracle`; the field
    /// here lets the dispatch JSON flip the oracle on/off per task.
    ///
    /// Defaults to `true` for `TaskRun` automatons because every
    /// dev-loop task declares a `test_command`, and the hint is cheap
    /// when the gate does not pass. Operators / dispatch authors can
    /// set `"early_test_oracle": false` to disable it for a specific
    /// task (e.g. ad-hoc chat-shaped runs that should not surface the
    /// hint).
    ///
    /// Plumbing the field into the live agent loop is the documented
    /// follow-up; today the field is parsed and stored so the
    /// downstream patch can flip the executor on without re-touching
    /// every dispatch site.
    #[allow(dead_code)]
    early_test_oracle: bool,
}

impl TaskRunConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, AutomatonError> {
        let project_id = config
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing project_id".into()))?
            .to_string();
        let task_id = config
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing task_id".into()))?
            .to_string();
        let agent_instance_id = config
            .get("agent_instance_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let agent_identity = super::dev_loop::AgentIdentityEnvelope::from_json(config);
        let early_test_oracle = config
            .get("early_test_oracle")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        Ok(Self {
            project_id,
            task_id,
            agent_instance_id,
            agent_identity,
            early_test_oracle,
        })
    }
}

#[async_trait::async_trait]
impl Automaton for TaskRunAutomaton {
    fn kind(&self) -> &'static str {
        "task-run"
    }

    fn default_schedule(&self) -> Schedule {
        Schedule::OnDemand
    }

    async fn tick(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        if self.tool_executor.is_none() {
            return Err(AutomatonError::InvalidConfig(
                "no tool executor configured — the agent cannot perform file or command operations"
                    .into(),
            ));
        }

        let cfg = TaskRunConfig::from_json(&ctx.config)?;
        let (task, project, spec) = self.fetch_task_context(&cfg).await?;

        ctx.emit(AutomatonEvent::TaskStarted {
            task_id: task.id.clone(),
            task_title: task.title.clone(),
        })?;

        if let Err(e) = self
            .domain
            .transition_task(&task.id, "in_progress", None)
            .await
        {
            warn!(task_id = %task.id, error = %e, "Failed to transition task to in_progress (continuing anyway)");
        }

        let result = self
            .run_agentic_task(ctx, &cfg, &project, &spec, &task)
            .await;
        self.finalize_task(ctx, &task.id, result).await
    }
}

impl TaskRunAutomaton {
    async fn fetch_task_context(
        &self,
        cfg: &TaskRunConfig,
    ) -> Result<
        (
            aura_tools::domain_tools::TaskDescriptor,
            aura_tools::domain_tools::ProjectDescriptor,
            aura_tools::domain_tools::SpecDescriptor,
        ),
        AutomatonError,
    > {
        let task = self
            .domain
            .get_task(&cfg.task_id, None)
            .await
            .map_err(|e| AutomatonError::DomainApi(e.to_string()))?;

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

        Ok((task, project, spec))
    }

    async fn run_agentic_task(
        &self,
        ctx: &TickContext,
        cfg: &TaskRunConfig,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        spec: &aura_tools::domain_tools::SpecDescriptor,
        task: &aura_tools::domain_tools::TaskDescriptor,
    ) -> Result<aura_agent::agent_runner::TaskExecutionResult, AutomatonError> {
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
        // wire fields are absent / blank.
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

        // Advisory drain: same pattern as `dev_loop::tick::execute_task`
        // and `chat::run_chat_loop`. See `dev_loop::forward_event` for
        // the post-E.4 drop policy that keeps the high-cadence
        // streaming-pump events from flooding the operator log.
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);
        let _forwarder = super::dev_loop::spawn_agent_event_forwarder(
            ctx.event_tx.clone(),
            event_rx,
            Some(task.id.clone()),
        );

        let cancel = ctx.cancellation_token().clone();
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

    async fn finalize_task(
        &self,
        ctx: &mut TickContext,
        task_id: &str,
        result: Result<aura_agent::agent_runner::TaskExecutionResult, AutomatonError>,
    ) -> Result<TickOutcome, AutomatonError> {
        // User-initiated stop fires the shared cancellation token. The agent
        // loop unwinds with either an empty `Ok(TaskExecutionResult)` or an
        // `Err(AgentExecution(...))` carrying the cancelled-stream message.
        // Either way it is NOT a task failure: report it cleanly so the audit
        // trail shows "cancelled by stop" instead of "task execution failed"
        // and skip transitioning the task to `failed` (rolling back to
        // `ready` mirrors the dev-loop behaviour and leaves the task in a
        // state the next run can pick up).
        if ctx.is_cancelled() {
            info!(
                automaton_id = %ctx.automaton_id,
                task_id,
                "Task run cancelled by user stop"
            );
            if let Err(te) = self.domain.transition_task(task_id, "ready", None).await {
                warn!(
                    task_id,
                    error = %te,
                    "Failed to roll cancelled task back to ready"
                );
            }
            ctx.emit(AutomatonEvent::LogLine {
                message: format!("Task {task_id} cancelled by stop request"),
            })?;
            return Ok(TickOutcome::Done);
        }

        match result {
            Ok(exec) => {
                if let Err(e) = self.domain.transition_task(task_id, "done", None).await {
                    warn!(task_id, error = %e, "Failed to transition task to done");
                }

                info!(task_id, notes = %exec.notes, "task completed");

                ctx.emit(AutomatonEvent::TaskCompleted {
                    task_id: task_id.to_string(),
                    summary: exec.notes,
                })?;
                ctx.emit(AutomatonEvent::TokenUsage {
                    task_id: Some(task_id.to_string()),
                    input_tokens: exec.input_tokens,
                    output_tokens: exec.output_tokens,
                })?;
            }
            Err(e) => {
                error!(task_id, error = %e, "task execution failed");

                if let Err(te) = self.domain.transition_task(task_id, "failed", None).await {
                    warn!(task_id, error = %te, "Failed to transition task to failed");
                }

                ctx.emit(AutomatonEvent::TaskFailed {
                    task_id: task_id.to_string(),
                    reason: e.to_string(),
                })?;
            }
        }

        Ok(TickOutcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::TaskRunConfig;
    use serde_json::json;

    #[test]
    fn from_json_parses_minimal_config() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
        }))
        .expect("parse minimal config");
        assert_eq!(cfg.project_id, "proj-1");
        assert_eq!(cfg.task_id, "task-1");
        assert_eq!(cfg.agent_instance_id, "default");
    }

    #[test]
    fn from_json_uses_provided_agent_instance_id() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
            "agent_instance_id": "inst-7",
        }))
        .expect("parse with explicit instance id");
        assert_eq!(cfg.agent_instance_id, "inst-7");
    }

    #[test]
    fn from_json_missing_project_id_errors() {
        let err =
            TaskRunConfig::from_json(&json!({ "task_id": "task-1" })).expect_err("missing field");
        assert!(err.to_string().to_lowercase().contains("project_id"));
    }

    #[test]
    fn from_json_missing_task_id_errors() {
        let err = TaskRunConfig::from_json(&json!({ "project_id": "proj-1" }))
            .expect_err("missing field");
        assert!(err.to_string().to_lowercase().contains("task_id"));
    }

    #[test]
    fn from_json_defaults_early_test_oracle_to_true() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
        }))
        .expect("parse minimal config");
        assert!(
            cfg.early_test_oracle,
            "Phase 3a oracle must default to ON for TaskRun automatons so every dev-loop task gets the hint without dispatch-side changes"
        );
    }

    #[test]
    fn from_json_respects_explicit_early_test_oracle_false() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
            "early_test_oracle": false,
        }))
        .expect("parse with explicit oracle override");
        assert!(
            !cfg.early_test_oracle,
            "explicit false in dispatch JSON must disable the oracle for this task"
        );
    }
}
