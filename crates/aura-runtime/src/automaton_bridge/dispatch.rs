//! Public entry-points that install automatons into the runtime.
//!
//! These are the methods `AutomatonController::start_dev_loop` /
//! `run_task` (via `mod.rs`) ultimately delegate to. They handle:
//!
//! 1. Per-project re-entrancy checks (only one dev loop per project).
//! 2. Tool/integration filtering (delegated to
//!    [`AutomatonBridge::prepare_installed_tools`] in [`super::build`]).
//! 3. Per-agent kernel construction (delegated to
//!    [`AutomatonBridge::build_kernel`] in [`super::build`]).
//! 4. Recording runtime capabilities for downstream debugging.
//! 5. Wiring the gateway domain so automaton-driven mutations land in
//!    the record log as `System::DomainMutation` (Invariant §2 / §8).
//! 6. Installing the automaton, recording the lifecycle event, and
//!    spawning the replay-aware event forwarder.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::agent_runner::AgentRunnerConfig;
use aura_agent::{KernelDomainGateway, KernelModelGateway, KernelToolGateway};
use aura_automaton::{DevLoopAutomaton, TaskRunAutomaton};
use aura_core::AgentPermissions;
use aura_kernel::Kernel;
use aura_protocol::AgentIdentityWire;
use aura_tools::catalog::ToolCatalog;
use aura_tools::domain_tools::DomainApi;
use tracing::{info, warn};

use crate::protocol::installed_integration_to_core;
use crate::runtime_capabilities;

use super::{AutomatonBridge, ProjectHandle};

/// Bootstrapping artefacts shared by every automaton kickoff path
/// (`start_dev_loop_with_capabilities`, `run_task_with_capabilities`,
/// future entry-points). Populated by [`AutomatonBridge::prepare_automaton_run`].
struct AutomatonRunContext {
    kernel: Arc<Kernel>,
    /// Held as the concrete [`KernelModelGateway`] type (rather than
    /// `Arc<dyn ModelProvider>`) so the automaton constructors'
    /// sealed [`aura_agent::RecordingModelProvider`] bound is satisfied
    /// at the call site. This makes the §1 "Sole External Gateway"
    /// invariant a structural property of the dispatch path: nothing
    /// in this file can hand a raw HTTP provider to an automaton.
    model_gw: Arc<KernelModelGateway>,
    tool_gw: Arc<dyn aura_agent::AgentToolExecutor>,
    gateway_domain: Arc<dyn DomainApi>,
    runner_config: AgentRunnerConfig,
    catalog: Arc<ToolCatalog>,
    effective_workspace: Option<PathBuf>,
}

/// Operator-facing labels embedded in the bootstrap error messages.
///
/// Held as a small struct so we can pass two string slices through one
/// argument without growing [`AutomatonBridge::prepare_automaton_run`]
/// past the existing `too_many_arguments` allow. The labels are spliced
/// into the same error templates the original two functions used,
/// preserving log/observability strings character-for-character.
struct AutomatonKindLabels {
    /// Inserted into `failed to build {} kernel`. Uses different wording
    /// per kind: `"dev loop"` (so the message ends `... dev loop kernel`)
    /// vs `"task runtime"` (`... task runtime kernel`).
    kernel: &'static str,
    /// Inserted into `failed to record {} runtime capabilities`.
    /// `"dev loop"` for the long-running dev loop, plain `"task"` for
    /// one-shot task runs.
    capabilities: &'static str,
}

impl AutomatonBridge {
    /// Run the bootstrap pipeline shared by every automaton kickoff
    /// path: build the per-project kernel, record its runtime
    /// capabilities, then assemble the model/tool/domain gateways and
    /// per-run config the automaton constructors require.
    ///
    /// `labels` controls only the operator-facing error strings so the
    /// original `failed to build dev loop kernel` /
    /// `failed to build task runtime kernel` (and the matching
    /// capabilities error) messages remain identical to the pre-refactor
    /// implementation.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_automaton_run(
        &self,
        project_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<&str>,
        // No silent fallback — every dev-loop / task-run kickoff must
        // pin the user-selected model. The two public entry points
        // (`start_dev_loop_with_capabilities`,
        // `run_task_with_capabilities`) reject the start request with
        // `"missing model"` before reaching this helper when the
        // operator forgot to set one.
        model: &str,
        installed_tools: Option<Vec<aura_protocol::InstalledTool>>,
        installed_integrations: Option<Vec<aura_protocol::InstalledIntegration>>,
        agent_permissions: AgentPermissions,
        aura_org_id: Option<&str>,
        aura_session_id: Option<&str>,
        aura_agent_id: Option<&str>,
        labels: AutomatonKindLabels,
    ) -> Result<AutomatonRunContext, String> {
        let domain = self.domain_with_jwt(auth_token);
        let effective_workspace = workspace_root.clone();
        let ws_path = effective_workspace
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));
        let installed_integrations = installed_integrations
            .unwrap_or_default()
            .into_iter()
            .map(installed_integration_to_core)
            .collect::<Vec<_>>();
        let installed_tools =
            Self::prepare_installed_tools(installed_tools, &installed_integrations);

        let kernel = self
            .build_kernel(
                domain.clone(),
                auth_token,
                Some(project_id),
                ws_path,
                effective_workspace.is_some(),
                installed_tools.clone(),
                installed_integrations.clone(),
                agent_permissions,
            )
            .map_err(|e| format!("failed to build {} kernel: {e}", labels.kernel))?;
        if let Err(e) = runtime_capabilities::record_runtime_capabilities(
            &kernel,
            "automaton",
            None,
            &installed_tools,
            &installed_integrations,
        )
        .await
        {
            return Err(format!(
                "failed to record {} runtime capabilities: {e}",
                labels.capabilities
            ));
        }
        let model_gw: Arc<KernelModelGateway> = Arc::new(KernelModelGateway::new(kernel.clone()));
        let tool_gw: Arc<dyn aura_agent::AgentToolExecutor> =
            Arc::new(KernelToolGateway::new(kernel.clone()));
        // Wrap the domain so mutations driven by automaton orchestration
        // (not the LLM tool loop) route through `kernel.process_direct`
        // and produce `SystemKind::DomainMutation` record entries. The
        // raw `domain` is still used inside `build_kernel` for the
        // `DomainToolExecutor`, whose mutations are captured via
        // `ToolExecution` entries by the kernel itself.
        let gateway_domain: Arc<dyn DomainApi> =
            Arc::new(KernelDomainGateway::new(domain.clone(), kernel.clone()));

        // `project_id` doubles as `aura_project_id` on the chat /
        // task-extract paths (see `SessionState::wire_request_ctx`
        // -> `aura_project_id: self.project_id.clone()`), so feed
        // the same value here to keep the dev-loop / task-run wire
        // shape symmetric with chat.
        let runner_config = self.build_runner_config(
            model,
            auth_token,
            aura_org_id,
            aura_session_id,
            aura_agent_id,
            Some(project_id),
        );
        let catalog = Arc::new(
            self.catalog
                .with_installed_tools(aura_tools::catalog::ToolProfile::Engine, &installed_tools),
        );

        Ok(AutomatonRunContext {
            kernel,
            model_gw,
            tool_gw,
            gateway_domain,
            runner_config,
            catalog,
            effective_workspace,
        })
    }

    #[allow(clippy::too_many_arguments)] // TODO(W4): collapse dev-loop kickoff args.
    pub(crate) async fn start_dev_loop_with_capabilities(
        &self,
        project_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
        installed_tools: Option<Vec<aura_protocol::InstalledTool>>,
        installed_integrations: Option<Vec<aura_protocol::InstalledIntegration>>,
        agent_permissions: AgentPermissions,
        aura_org_id: Option<String>,
        aura_session_id: Option<String>,
        aura_agent_id: Option<String>,
        agent_identity: Option<AgentIdentityWire>,
        agent_skills: Vec<String>,
        agent_system_prompt: Option<String>,
    ) -> Result<String, String> {
        // Hard-fail when the operator did not pin a model. The
        // pre-fix dev-loop path silently fell back to a build-time
        // constant (`claude-opus-4-6`) here, which is exactly the
        // regression the worker-identity work is closing. We surface
        // the gap loudly so the caller (typically aura-os's
        // `POST /automaton/start`) sees the configuration mismatch
        // instead of routing eval traffic to an unintended model.
        let model = model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "missing model — dev loop start request must include an explicit model identifier"
                    .to_string()
            })?
            .to_string();

        if let Some(entry) = self.project_handles.get(project_id) {
            let tracked = entry.value();
            if !tracked.handle.is_finished()
                && self.runtime.get_info(&tracked.automaton_id).is_some()
            {
                return Err(format!(
                    "A dev loop is already running for project {project_id} (automaton_id: {})",
                    tracked.automaton_id
                ));
            }
            if !tracked.handle.is_finished() {
                warn!(
                    project_id,
                    automaton_id = %tracked.automaton_id,
                    "dropping stale dev-loop project handle with no matching runtime automaton"
                );
            }
            drop(entry);
            self.project_handles.remove(project_id);
        }

        let ctx = self
            .prepare_automaton_run(
                project_id,
                workspace_root,
                auth_token.as_deref(),
                &model,
                installed_tools,
                installed_integrations,
                agent_permissions,
                aura_org_id.as_deref(),
                aura_session_id.as_deref(),
                aura_agent_id.as_deref(),
                AutomatonKindLabels {
                    kernel: "dev loop",
                    capabilities: "dev loop",
                },
            )
            .await?;

        let automaton = DevLoopAutomaton::new(
            ctx.gateway_domain,
            ctx.model_gw,
            ctx.runner_config,
            ctx.catalog,
        )
        .with_tool_executor(ctx.tool_gw);

        let config = serde_json::json!({
            "project_id": project_id,
            "model": model,
            "git_repo_url": git_repo_url,
            "git_branch": git_branch,
            "auth_token": auth_token.as_deref(),
            // PR B: typed identity wire fields surface to the
            // automaton via the same JSON config blob the rest of the
            // dev-loop kickoff already uses. `DevLoopConfig::from_json`
            // parses them back out and threads them into
            // `AgenticTaskParams::agent`. `null` / `[]` defaults keep
            // the assembled prompt byte-identical with PR A until
            // aura-os populates these fields in PR C.
            "agent_identity": agent_identity,
            "agent_skills": agent_skills,
            "agent_system_prompt": agent_system_prompt,
        });

        let (handle, event_rx) = self
            .runtime
            .install(Box::new(automaton), config, ctx.effective_workspace)
            .await
            .map_err(|e| format!("failed to install dev-loop automaton: {e}"))?;

        let automaton_id = handle.id().as_str().to_string();
        // Register the dev-loop agent's identity BEFORE the
        // lifecycle-event nudge so the scheduler tick that runs as
        // part of `record_lifecycle_event` picks it up. The
        // dev-loop's first model call advertises
        // `DevLoopBootstrap`; the loop self-promotes to
        // `DevLoopContinuation` on subsequent iterations via the
        // automaton-side runner config, so the registry only needs
        // to seed the bootstrap kind for any pre-loop scheduling
        // wakeups.
        self.register_automaton_identity(
            ctx.kernel.agent_id,
            &model,
            auth_token.as_deref(),
            aura_org_id.as_deref(),
            aura_session_id.as_deref(),
            aura_agent_id.as_deref(),
            Some(project_id),
            aura_reasoner::ModelRequestKind::DevLoopBootstrap,
        );
        self.record_lifecycle_event(ctx.kernel.agent_id, &automaton_id, "start_dev_loop")
            .await;
        self.spawn_event_forwarder(automaton_id.clone(), event_rx);

        info!(project_id, automaton_id = %automaton_id, "Dev loop started");
        self.project_handles.insert(
            project_id.to_string(),
            ProjectHandle {
                automaton_id: automaton_id.clone(),
                agent_id: ctx.kernel.agent_id,
                handle,
            },
        );
        Ok(automaton_id)
    }

    #[allow(clippy::too_many_arguments)] // TODO(W4): collapse task-runner args.
    pub(crate) async fn run_task_with_capabilities(
        &self,
        project_id: &str,
        task_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
        installed_tools: Option<Vec<aura_protocol::InstalledTool>>,
        installed_integrations: Option<Vec<aura_protocol::InstalledIntegration>>,
        agent_permissions: AgentPermissions,
        prior_failure: Option<String>,
        work_log: Vec<String>,
        aura_org_id: Option<String>,
        aura_session_id: Option<String>,
        aura_agent_id: Option<String>,
        agent_identity: Option<AgentIdentityWire>,
        agent_skills: Vec<String>,
        agent_system_prompt: Option<String>,
    ) -> Result<String, String> {
        // Mirror the dev-loop entry-point: refuse to start without an
        // explicit model. Same regression rationale.
        let model = model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "missing model — task run request must include an explicit model identifier"
                    .to_string()
            })?
            .to_string();
        let ctx = self
            .prepare_automaton_run(
                project_id,
                workspace_root,
                auth_token.as_deref(),
                &model,
                installed_tools,
                installed_integrations,
                agent_permissions,
                aura_org_id.as_deref(),
                aura_session_id.as_deref(),
                aura_agent_id.as_deref(),
                AutomatonKindLabels {
                    kernel: "task runtime",
                    capabilities: "task",
                },
            )
            .await?;

        let automaton = TaskRunAutomaton::new(
            ctx.gateway_domain,
            ctx.model_gw,
            ctx.runner_config,
            ctx.catalog,
        )
        .with_tool_executor(ctx.tool_gw);

        let config = serde_json::json!({
            "project_id": project_id,
            "task_id": task_id,
            "model": model,
            "git_repo_url": git_repo_url,
            "git_branch": git_branch,
            "auth_token": auth_token.as_deref(),
            "prior_failure": prior_failure,
            "work_log": work_log,
            // PR B: same wire-field threading as `start_dev_loop_with_capabilities`.
            // Single-task kickoffs go through the same prompt assembly,
            // so they pick up identity / skills / operator system
            // prompt the same way once PR C populates them upstream.
            "agent_identity": agent_identity,
            "agent_skills": agent_skills,
            "agent_system_prompt": agent_system_prompt,
        });

        let (handle, event_rx) = self
            .runtime
            .install(Box::new(automaton), config, ctx.effective_workspace)
            .await
            .map_err(|e| format!("failed to install task-run automaton: {e}"))?;

        let automaton_id = handle.id().as_str().to_string();
        // Register the task-run agent's identity BEFORE the
        // lifecycle-event nudge. Task runs use the chat
        // request-kind on the worker fan-out path because they
        // don't enter the dev-loop self-promotion cycle.
        self.register_automaton_identity(
            ctx.kernel.agent_id,
            &model,
            auth_token.as_deref(),
            aura_org_id.as_deref(),
            aura_session_id.as_deref(),
            aura_agent_id.as_deref(),
            Some(project_id),
            aura_reasoner::ModelRequestKind::Chat,
        );
        self.record_lifecycle_event(ctx.kernel.agent_id, &automaton_id, "start_task_run")
            .await;
        self.spawn_event_forwarder(automaton_id.clone(), event_rx);

        info!(project_id, task_id, automaton_id = %automaton_id, "Task execution started (non-blocking)");
        Ok(automaton_id)
    }
}
