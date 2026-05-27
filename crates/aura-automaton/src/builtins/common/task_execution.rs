//! Shared agent-loop handoff used by `dev_loop/tick.rs` and
//! `task_run.rs`.
//!
//! Both automatons build a near-identical `AgenticTaskParams` payload,
//! spawn the same advisory event forwarder, and call
//! `AgentRunner::execute_task_tracked`. Pre-Phase-6 the two
//! implementations had drifted slightly (different fallback chains
//! for `effective_path`, different inner-channel sizes) and any
//! future tweak meant editing both sites. This module collapses them
//! into [`run_tracked_task`] so the per-automaton tick stays a thin
//! policy layer on top.

use std::sync::Arc;

use aura_agent::agent_runner::{
    AgentRunner, AgenticTaskParams, TaskExecutionResult, TaskTrackingConfig,
};
use aura_prompts::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};
use aura_reasoner::ModelProvider;
use aura_tools::catalog::{ToolCatalog, ToolProfile};
use aura_tools::domain_tools::{ProjectDescriptor, SpecDescriptor, TaskDescriptor};

use super::config::AgentIdentityEnvelope;
use super::forward_event::spawn_agent_event_forwarder;
use crate::builtins::noop_executor::NoOpExecutor;
use crate::context::TickContext;
use crate::error::AutomatonError;

/// All the inputs `dev_loop` and `task_run` need to hand off a single
/// task to `AgentRunner::execute_task_tracked`. Packed into a struct
/// so the public helper signature stays under the rules.md
/// parameter-count line and so Phase 8's `RunCtx` migration has a
/// natural seam to land on.
pub(crate) struct TaskExecutionRequest<'a> {
    /// Tick context — used for the advisory forwarder, cancellation,
    /// and the workspace-root override.
    pub(crate) ctx: &'a TickContext,
    /// Shared `AgentRunner` constructed once at automaton install
    /// time and reused across ticks. Borrowed rather than cloned
    /// because the dev-loop and task-run automatons both own it.
    pub(crate) runner: &'a AgentRunner,
    /// Kernel-mediated model provider stashed on the per-automaton
    /// struct.
    pub(crate) provider: &'a dyn ModelProvider,
    /// Tool catalog stashed on the per-automaton struct.
    pub(crate) catalog: &'a ToolCatalog,
    /// Already-resolved task / spec / project descriptors. The
    /// caller is responsible for fetching them via `DomainApi` and
    /// applying any policy gates (e.g. dev-loop's "skip `done`
    /// tasks" filter) before calling here.
    pub(crate) task: &'a TaskDescriptor,
    pub(crate) spec: &'a SpecDescriptor,
    pub(crate) project: &'a ProjectDescriptor,
    /// Identity envelope to project onto
    /// [`AgenticTaskParams::agent`]. Empty envelopes render as the
    /// historical no-identity baseline.
    pub(crate) identity: &'a AgentIdentityEnvelope,
    /// Underlying tool executor (file ops, shell, etc.). Wrapped
    /// internally by `TaskToolExecutor`. When `None` the helper
    /// substitutes a [`NoOpExecutor`] so the agent still loops; the
    /// caller is responsible for the up-front "no executor
    /// configured" hard-fail policy if one is desired.
    pub(crate) tool_executor: Option<Arc<dyn aura_agent::types::AgentToolExecutor>>,
    /// Per-task override for the early-test-oracle steering source.
    /// `None` defers to `AgentRunnerConfig.early_test_oracle`;
    /// `Some(v)` overrides it for this call.
    pub(crate) early_test_oracle: Option<bool>,
    /// Optional build-retry note appended to the task description.
    /// Used by the dev-loop's "build still red after first pass"
    /// retry path; `None` for the task-run automaton.
    pub(crate) build_retry_note: Option<String>,
}

/// Execute one tracked agent task with the request-struct inputs.
///
/// Wraps the common pattern shared by `dev_loop` and `task_run`:
///
/// 1. Project an `AgenticTaskParams` from the descriptors.
/// 2. Build the project-scoped `ProjectInfo` / `SpecInfo` / `TaskInfo`
///    views (the prompt builders need `&str` borrows).
/// 3. Spawn the advisory `forward_agent_event` task drain so
///    `TextDelta` / `ThinkingDelta` / `ToolStart` events reach the
///    automaton WS channel.
/// 4. Hand off to `AgentRunner::execute_task_tracked` and surface any
///    failure as [`AutomatonError::AgentExecution`] with the task id
///    attached.
///
/// The advisory forwarder's `JoinHandle` is intentionally dropped:
/// the spawned task exits when its inner `event_tx` is dropped at
/// the end of `execute_task_tracked`, so there's no leak.
pub(crate) async fn run_tracked_task(
    request: TaskExecutionRequest<'_>,
) -> Result<TaskExecutionResult, AutomatonError> {
    let TaskExecutionRequest {
        ctx,
        runner,
        provider,
        catalog,
        task,
        spec,
        project,
        identity,
        tool_executor,
        early_test_oracle,
        build_retry_note,
    } = request;

    let effective_path = ctx
        .workspace_root_str()
        .unwrap_or_else(|| project.path.clone());

    let mut task_description = task.description.clone();
    if let Some(note) = build_retry_note.as_deref() {
        task_description.push_str("\n\n---\n\nBuild still failing after your last pass:\n\n");
        task_description.push_str(note);
    }

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
        description: &task_description,
        execution_notes: "",
        files_changed: &[],
    };
    let session_info = SessionInfo {
        summary_of_previous_context: "",
    };

    let tools = catalog.tools_for_profile(ToolProfile::Engine);

    // Borrow the parsed identity envelope (if any) as a transient
    // `AgentInfo<'_>` so `SystemPromptBuilder` renders the
    // `<agent_identity>` / `<agent_skills>` / `<agent_system_prompt>`
    // sections. Empty envelopes leave the prompt byte-identical to
    // the no-identity baseline.
    let agent_info = identity.as_agent_info();

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
    // `ToolInputSnapshot` / `ToolCallCompleted` / `ToolResult`) here
    // at a high cadence on the E.4 streaming-pump path. The forwarder
    // consumes them and projects through `forward_agent_event` onto
    // the outer automaton channel. See `forward_event.rs` for the
    // post-E.4 drop policy that keeps this from flooding the operator
    // log when the outer consumer is briefly behind or has already
    // torn down.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);
    let _forwarder = spawn_agent_event_forwarder(
        ctx.forwarder_sender_clone(),
        event_rx,
        Some(task.id.clone()),
    );

    let inner_executor: Arc<dyn aura_agent::types::AgentToolExecutor> =
        tool_executor.unwrap_or_else(|| Arc::new(NoOpExecutor));

    let tracking = TaskTrackingConfig {
        inner_executor,
        project_folder: effective_path.clone(),
        build_command: project.build_command.clone(),
        test_command: project.test_command.clone(),
        early_test_oracle,
    };

    let cancel = ctx.cancellation_token().clone();
    runner
        .execute_task_tracked(provider, tracking, &params, Some(event_tx), Some(cancel))
        .await
        .map_err(|e| AutomatonError::agent_execution(Some(task.id.clone()), e))
}
