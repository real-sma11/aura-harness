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
//! retry warm-up plumbing. Phase 6 hard-fails on a missing `model`
//! field in the dispatch JSON (parity with `dev_loop` and
//! `spec_gen`), shares the
//! [`super::common::run_tracked_task`] handoff with `dev_loop`, and
//! routes the success / failure / cancel transitions through
//! [`super::common::finalize_task_outcome`].

use std::sync::Arc;

use tracing::warn;

use aura_agent::agent_runner::{AgentRunner, AgentRunnerConfig};
use aura_reasoner::ModelProvider;
use aura_tools::catalog::ToolCatalog;
use aura_tools::domain_tools::DomainApi;

use super::common::{
    finalize_task_outcome, run_tracked_task, AgentIdentityEnvelope, TaskExecutionRequest,
};
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
    /// Model identifier from the dispatch JSON. Mirrors the
    /// `dev_loop` / `spec_gen` policy: a missing or blank `model`
    /// field is a typed `InvalidConfig` failure rather than a silent
    /// fall-through. Pre-Phase-6 this field accepted an empty string
    /// and downstream refinement saw `""` — the same routing
    /// regression where the `claude-opus-4-7` selection got swapped
    /// for `claude-opus-4-6` because the omission was silent. Phase 6
    /// aligns all three automatons on the hard-fail policy.
    model: String,
    /// Identity envelope parsed off the dispatch JSON. Reuses the
    /// `AgentIdentityEnvelope` from `crate::builtins::common::config`
    /// so the two automatons share a single parser + render pathway.
    /// Stays empty (`is_empty == true`) until the aura-os populator
    /// lands.
    agent_identity: AgentIdentityEnvelope,
    /// Phase 3a (reread-efficiency plan): opt-in switch for the early
    /// "is the test gate already green?" oracle. The state machine
    /// and steering kind live in
    /// `aura_agent::agent_loop::steering::EarlyTestOracle`; the field
    /// here lets the dispatch JSON flip the oracle on/off per task.
    ///
    /// Defaults to `true` for `TaskRun` automatons because every
    /// dev-loop task declares a `test_command`, and the hint is cheap
    /// when the gate does not pass. Operators / dispatch authors can
    /// set `"early_test_oracle": false` to disable it for a specific
    /// task (e.g. ad-hoc chat-shaped runs that should not surface the
    /// hint).
    ///
    /// Phase 5 of the core-loop architecture refactor wired this
    /// through: the value flows into
    /// [`aura_agent::agent_runner::AgentRunnerConfig::early_test_oracle`]
    /// at runner construction time and ultimately installs the
    /// `EarlyTestOracle` source into the per-run `SteeringRegistry`.
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
        // Phase 6: align with `dev_loop` / `spec_gen` — hard-fail on
        // a missing model identifier instead of silently propagating
        // `""` into the refinement call.
        let model = config
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AutomatonError::InvalidConfig(
                    "missing model — task-run requires an explicit model identifier in the start request".into(),
                )
            })?
            .to_string();
        let agent_identity = AgentIdentityEnvelope::from_json(config);
        let early_test_oracle = config
            .get("early_test_oracle")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        Ok(Self {
            project_id,
            task_id,
            agent_instance_id,
            model,
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
        let _outcome = finalize_task_outcome(ctx, self.domain.as_ref(), &task, result).await?;
        Ok(TickOutcome::Done)
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
            .map_err(|e| AutomatonError::domain_api(Some(cfg.task_id.clone()), e))?;

        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(Some(cfg.task_id.clone()), e))?;

        let spec = self
            .domain
            .get_spec(&task.spec_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(Some(cfg.task_id.clone()), e))?;

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
        // Pre-implementation refinement. The single-task path has no
        // build-retry wrapper (unlike the dev loop), so we always
        // attempt refinement here; the helper's idempotency marker
        // makes repeated runs of the same task safe.
        let task_owned = crate::builtins::task_refinement::refine_task_description(
            self.domain.as_ref(),
            self.provider.as_ref(),
            &cfg.model,
            spec,
            task,
            Some(ctx.event_sender()),
        )
        .await?;

        run_tracked_task(TaskExecutionRequest {
            ctx,
            runner: &self.runner,
            provider: self.provider.as_ref(),
            catalog: self.catalog.as_ref(),
            task: &task_owned,
            spec,
            project,
            identity: &cfg.agent_identity,
            tool_executor: self.tool_executor.clone(),
            // Phase 5: forward the per-task switch parsed off the
            // dispatch JSON so the agent loop installs the
            // `EarlyTestOracle` source into the per-run
            // `SteeringRegistry`.
            early_test_oracle: Some(cfg.early_test_oracle),
            build_retry_note: None,
        })
        .await
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
            "model": "claude-opus-4-7",
        }))
        .expect("parse minimal config");
        assert_eq!(cfg.project_id, "proj-1");
        assert_eq!(cfg.task_id, "task-1");
        assert_eq!(cfg.agent_instance_id, "default");
        assert_eq!(cfg.model, "claude-opus-4-7");
    }

    #[test]
    fn from_json_uses_provided_agent_instance_id() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
            "agent_instance_id": "inst-7",
            "model": "claude-opus-4-7",
        }))
        .expect("parse with explicit instance id");
        assert_eq!(cfg.agent_instance_id, "inst-7");
    }

    #[test]
    fn from_json_missing_project_id_errors() {
        let err = TaskRunConfig::from_json(&json!({
            "task_id": "task-1",
            "model": "claude-opus-4-7",
        }))
        .expect_err("missing field");
        assert!(err.to_string().to_lowercase().contains("project_id"));
    }

    #[test]
    fn from_json_missing_task_id_errors() {
        let err = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "model": "claude-opus-4-7",
        }))
        .expect_err("missing field");
        assert!(err.to_string().to_lowercase().contains("task_id"));
    }

    /// Phase 6 model-required policy: align task-run with dev_loop /
    /// spec_gen. Pre-Phase-6 the field defaulted to `""` and the
    /// refinement call silently fell through with an empty model
    /// string — the same root-cause shape as the
    /// `claude-opus-4-7` → `claude-opus-4-6` regression. The
    /// hard-fail surfaces the configuration gap up front instead of
    /// burning provider budget on a half-configured run.
    #[test]
    fn from_json_missing_model_errors() {
        let err = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
        }))
        .expect_err("missing model must error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("model"),
            "error must mention the missing model field, got: {msg}"
        );
    }

    #[test]
    fn from_json_blank_model_errors() {
        let err = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
            "model": "   ",
        }))
        .expect_err("blank model must error");
        assert!(err.to_string().to_lowercase().contains("model"));
    }

    #[test]
    fn from_json_defaults_early_test_oracle_to_true() {
        let cfg = TaskRunConfig::from_json(&json!({
            "project_id": "proj-1",
            "task_id": "task-1",
            "model": "claude-opus-4-7",
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
            "model": "claude-opus-4-7",
            "early_test_oracle": false,
        }))
        .expect("parse with explicit oracle override");
        assert!(
            !cfg.early_test_oracle,
            "explicit false in dispatch JSON must disable the oracle for this task"
        );
    }
}
