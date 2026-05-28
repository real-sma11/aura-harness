//! High-level agent execution: agentic task, chat, and shell-task runners.
//!
//! `AgentRunner` combines task context setup, agent loop configuration, and
//! result processing into a convenient orchestration layer built on top of
//! [`AgentLoop`].

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use aura_reasoner::{Message, ModelProvider, ModelRequestKind, ToolDefinition};

use crate::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::events::AgentLoopEvent;
use crate::planning::{TaskPhase, TaskPlan};
use aura_prompts::bootstrap::build_agentic_task_context;
use aura_prompts::{
    agentic_execution_system_prompt, build_chat_system_prompt, AgentInfo, ProjectInfo, SessionInfo,
    SpecInfo, TaskInfo,
};

use crate::prompt_resolve::{default_caps, extract_hints, resolve_hints, FsWorkspace};
use crate::task_context;
use crate::task_executor::TaskToolExecutor;
use crate::turn_config::{classify_task_complexity, resolve_simple_model, TaskComplexity};
use crate::types::{AgentLoopResult, AgentToolExecutor};
use crate::verify::{
    auto_correct_build_command, normalize_error_signature, run_build_command, BuildFixAttemptRecord,
};

// ---------------------------------------------------------------------------
// Internal call-shape helpers
// ---------------------------------------------------------------------------

/// Phase 8 bundle of the per-call options that distinguish the
/// `execute_task` and `execute_task_tracked` entry points when both
/// funnel through [`AgentRunner::execute_task_inner`]. Carries the
/// optional phase-reset signal (tracked path), the optional
/// pre-built task context bundle (tracked path), and the per-call
/// `early_test_oracle` toggle (either runner default or per-task
/// override). Bundling keeps `execute_task_inner` under the
/// `too-many-arguments` ceiling without flattening the optional
/// fields into the public API.
struct TaskInnerOptions {
    phase_reset_signal: Option<Arc<AtomicBool>>,
    prebuilt_task_ctx: Option<String>,
    early_test_oracle: bool,
}

/// Phase 8 bundle of the prompt-shaping inputs for
/// [`AgentRunner::execute_chat`]. Both fields feed the chat-system
/// prompt builder; grouping them keeps the public `execute_chat`
/// signature under the clippy ceiling without losing the per-caller
/// flexibility (the wider call sites still pass an owned
/// [`ProjectInfo`] and a `&str` prompt body).
pub struct ChatPromptCtx<'a> {
    pub project: &'a ProjectInfo<'a>,
    pub custom_system_prompt: &'a str,
}

/// Phase 8 bundle of the optional coordination handles for
/// [`AgentRunner::execute_chat`]. Pairs the streaming
/// [`AgentLoopEvent`] sink with the external cancellation signal so
/// the public `execute_chat` signature stays under the
/// `clippy::too_many_arguments` ceiling without forcing every
/// caller to thread two trailing `Option<…>` arguments.
#[derive(Default)]
pub struct ChatHooks {
    pub event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
    pub cancel: Option<CancellationToken>,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of executing an agentic task.
///
/// Phase 7 dropped the `no_changes_needed`, `file_ops`, and
/// `reached_implementing` fields: the automaton finalizer
/// (`builtins::common::finalize::finalize_task_outcome`) only
/// consumes `notes` + `input_tokens` + `output_tokens` on the
/// success path, and the corresponding `CommitSkipped` /
/// `FileOpsApplied` events that would have consumed the dropped
/// fields had no emitters or external consumers in this workspace.
/// The internal `TaskToolExecutor.tracked_file_ops` /
/// `no_changes_needed` / `task_phase` state still drives the
/// `task_done` rejection semantics inside the loop — that surface
/// is independent of this struct.
#[derive(Debug, Clone, Default)]
pub struct TaskExecutionResult {
    pub notes: String,
    pub follow_up_tasks: Vec<FollowUpSuggestion>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub files_already_applied: bool,
    /// Final message history from the agent loop. Downstream validators use
    /// this to build recovery hints (e.g. which file paths the agent tried
    /// to write before truncation).
    pub messages: Vec<aura_reasoner::Message>,
}

/// Suggested follow-up task from agent execution.
///
/// `Serialize` / `Deserialize` are derived because this type travels
/// through JSON execution-response parsing. The parser historically
/// carried its own copy of the struct and converted at the boundary,
/// which meant a field rename silently dropped the field on one side
/// of the copy. Phase 3 consolidated the two definitions here; Phase
/// 4e deleted the unused `parser` module.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FollowUpSuggestion {
    pub title: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the `AgentRunner`.
#[derive(Debug, Clone)]
pub struct AgentRunnerConfig {
    pub max_agentic_iterations: usize,
    pub max_shell_task_retries: u32,
    pub task_execution_max_tokens: u32,
    pub thinking_budget: u32,
    pub stream_timeout_secs: u64,
    pub max_context_tokens: u64,
    pub max_task_credits: Option<u64>,
    pub default_model: String,
    pub simple_model: String,
    /// JWT auth token for proxy-mode LLM routing.
    pub auth_token: Option<String>,
    /// Org UUID forwarded as the `X-Aura-Org-Id` header on outbound
    /// `/v1/messages` calls so `aura-router` can bucket per-org rate
    /// limits / billing on automaton runs the same way it does for
    /// interactive chat (where `SessionState::aura_org_id` already
    /// flows through `AgentLoopConfig::aura_org_id`). Without this
    /// header `aura-router` falls back to per-IP buckets, which is
    /// what makes burst-y eval automation traffic trip Cloudflare on
    /// `aura-router.onrender.com` even when chat from the same
    /// account does not.
    pub aura_org_id: Option<String>,
    /// Storage session UUID forwarded as the `X-Aura-Session-Id`
    /// header on outbound `/v1/messages` calls. Mirrors the chat
    /// path's `SessionState::aura_session_id`, generated per
    /// automaton-start so concurrent runs of the same agent get
    /// distinct billing/observability partitions.
    pub aura_session_id: Option<String>,
    /// Template agent UUID forwarded as the `X-Aura-Agent-Id` header
    /// on outbound `/v1/messages` calls. Mirrors the chat path's
    /// `SessionState::aura_agent_id` (populated from
    /// `RuntimeRequest.project.aura_agent_id`). Without this, automaton runs
    /// hit `aura-router` without an agent identity and Cloudflare's
    /// WAF treats them as unsanctioned API traffic, returning the
    /// HTML challenge page (status 403) that's been stalling SWE-bench.
    pub aura_agent_id: Option<String>,
    /// Project UUID forwarded as the `X-Aura-Project-Id` header on
    /// outbound `/v1/messages` calls. Mirrors the chat / task-extract
    /// paths' `SessionState::project_id` -> `aura_project_id`
    /// projection. Same WAF-bucketing rationale as
    /// [`Self::aura_agent_id`].
    pub aura_project_id: Option<String>,
    /// Stable `prompt_cache_key` forwarded to OpenAI-family routing
    /// via [`AgentLoopConfig::prompt_cache_key`]. The dev-loop and
    /// task-run paths set this to `Some(format!("devloop:{project_id}"))`
    /// so cross-task agent invocations land on the same OpenAI cache
    /// bucket. Anthropic caching does not need a key — the provider's
    /// ephemeral `cache_control` breakpoints in
    /// `aura_reasoner::anthropic::convert` handle prefix reuse.
    pub prompt_cache_key: Option<String>,
    /// Phase 5: per-task switch for the early test-gate oracle.
    ///
    /// `TaskRunAutomaton` (and any future task-shaped automaton) sets
    /// this `true` to install
    /// [`crate::agent_loop::steering::EarlyTestOracle`] into the
    /// per-run `SteeringRegistry` for every task this runner
    /// executes; chat / shell / generic callers leave it `false` and
    /// the oracle never fires.
    ///
    /// The wired-through path is:
    /// `TaskRunConfig.early_test_oracle` →
    /// `AgentRunnerConfig.early_test_oracle` →
    /// `AgentLoopConfig.early_test_oracle: Option<EarlyTestOracleConfig>`
    /// → `LoopState::steering`.
    pub early_test_oracle: bool,
}

impl AgentRunnerConfig {
    /// Construct an [`AgentRunnerConfig`] for an explicit, caller-supplied
    /// model. **There is no `Default` impl on purpose**: silently
    /// substituting a constant is exactly what shipped the wrong
    /// upstream model in production (the worker path took
    /// `AgentRunnerConfig::default()` and routed every dev-loop / task-run
    /// turn at `claude-opus-4-6` even when the user had selected
    /// `claude-opus-4-7`).
    ///
    /// `simple_model` defaults to `model` so callers that don't have a
    /// distinct cheap-tier model still get a sane starting point; pass
    /// it through [`Self::with_simple_model`] when a separate model
    /// should drive the trivial-task path.
    #[must_use]
    pub fn for_agent(model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            max_agentic_iterations: aura_core::MAX_TURNS as usize,
            max_shell_task_retries: 4,
            task_execution_max_tokens: 16_384,
            // Stripped (2026-05): cut from 10_000 to 2_000.
            // Phase 2 of harness-v2 (2026-05 round 3): further reduced
            // from 2000 to 800. Extended-thinking turns produced no
            // faster convergence — they just stretched per-turn
            // latency, and the tasks that timed out were the same
            // ones that loop on read-only tool calls regardless of
            // how much budget the model has to deliberate. The
            // explore turn should be fast tool calls, not
            // multi-minute deliberation; the iteration-0 disable in
            // `LoopState::begin_iteration` clamps it further for the
            // very first turn. See `harness_task_completion_fix` plan.
            thinking_budget: 800,
            // Matches the reasoner's default reqwest request timeout
            // (300s / `AURA_MODEL_TIMEOUT_MS`) so the outer `timeout()`
            // guard in `AgentLoop::call_model` does not preempt an
            // in-flight provider request. See the comment on
            // `AgentLoopConfig::stream_timeout`.
            stream_timeout_secs: 300,
            max_context_tokens: 200_000,
            max_task_credits: None,
            simple_model: model.clone(),
            default_model: model,
            auth_token: None,
            aura_org_id: None,
            aura_session_id: None,
            aura_agent_id: None,
            aura_project_id: None,
            prompt_cache_key: None,
            // Phase 5: default off so chat / generic runners do not
            // pay for the oracle's hint generation. Task-shaped
            // automatons flip this on per-construction.
            early_test_oracle: false,
        }
    }

    /// Override the simple-task model used for trivial classifications
    /// (see [`crate::turn_config::resolve_simple_model`]). Callers that
    /// don't care fall through to [`Self::for_agent`]'s default of
    /// echoing the primary model.
    #[must_use]
    pub fn with_simple_model(mut self, simple: impl Into<String>) -> Self {
        self.simple_model = simple.into();
        self
    }

    /// Set the `early_test_oracle` switch. Task-shaped automatons
    /// (`TaskRunAutomaton`, `DevLoopAutomaton`) call this from their
    /// dispatch JSON parser; chat / shell / generic runners leave the
    /// default (`false`) in place.
    #[must_use]
    pub fn with_early_test_oracle(mut self, enabled: bool) -> Self {
        self.early_test_oracle = enabled;
        self
    }
}

/// Parameters for running an agentic task.
pub struct AgenticTaskParams<'a> {
    pub project: &'a ProjectInfo<'a>,
    pub spec: &'a SpecInfo<'a>,
    pub task: &'a TaskInfo<'a>,
    pub session: &'a SessionInfo<'a>,
    pub work_log: &'a [String],
    pub completed_deps: &'a [TaskInfo<'a>],
    pub workspace_map: &'a str,
    pub codebase_snapshot: &'a str,
    pub type_defs_context: &'a str,
    pub dep_api_context: &'a str,
    pub member_count: usize,
    pub tools: Vec<ToolDefinition>,
    /// 0-indexed retry counter. `0` is the first attempt (a fresh task
    /// or an initial run after a process restart), `1+` is a retry.
    /// Gates Phase 4 context enrichment: pre-resolved paths/symbols are
    /// only spliced into the initial user message on attempt 0, since
    /// retries get a different, narrower context from Phase 5's
    /// decomposition path (and re-injecting the same hints wastes
    /// tokens without adding signal).
    pub attempt: u32,
    /// Agent identity / skills / operator-authored system prompt
    /// threaded from the wire layer (PR B re-adds the field that PR A
    /// removed alongside the dead `build_agent_preamble`).
    ///
    /// PR B: every construction site passes `None`. The
    /// `agentic_execution_system_prompt` builder is wired to consume
    /// the bundle so PR C's aura-os populator can flip a single field
    /// and have identity flow into the model-facing prompt.
    pub agent: Option<&'a AgentInfo<'a>>,
}

/// Context for a shell task execution.
pub struct ShellTaskParams<'a> {
    pub command: &'a str,
    pub project_root: &'a Path,
}

/// Configuration for task-aware tracking in [`AgentRunner::execute_task_tracked`].
///
/// Bundles the inner tool executor and project metadata that `TaskToolExecutor`
/// needs so that callers do not have to construct the executor themselves.
pub struct TaskTrackingConfig {
    /// Inner executor that handles filesystem and search tools.
    pub inner_executor: Arc<dyn AgentToolExecutor>,
    /// Path to the project root for build and stub checks.
    pub project_folder: String,
    /// Build command (from project config or auto-detected).
    pub build_command: Option<String>,
    /// Test command used by the `task_done` hard gate. Forwarded directly
    /// onto [`crate::task_executor::TaskToolExecutor::test_command`].
    pub test_command: Option<String>,
    /// Phase 5: per-task switch for the early test-gate oracle.
    /// `None` falls back to [`AgentRunnerConfig::early_test_oracle`];
    /// `Some(v)` overrides it for this one task. The `TaskRunAutomaton`
    /// populates this from `TaskRunConfig.early_test_oracle` parsed
    /// off the dispatch JSON so per-task opt-outs (e.g. ad-hoc chat-shaped
    /// runs) are honored without rebuilding the shared
    /// [`AgentRunner`].
    pub early_test_oracle: Option<bool>,
}

// ---------------------------------------------------------------------------
// AgentRunner
// ---------------------------------------------------------------------------

/// High-level runner that configures and executes agent loops for tasks,
/// chat sessions, and shell commands.
pub struct AgentRunner {
    pub config: AgentRunnerConfig,
}

impl AgentRunner {
    #[must_use]
    pub const fn new(config: AgentRunnerConfig) -> Self {
        Self { config }
    }

    /// Execute an agentic task: build context, configure the loop, run it,
    /// and process results.
    pub async fn execute_task(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        params: &AgenticTaskParams<'_>,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancel: Option<CancellationToken>,
    ) -> Result<TaskExecutionResult, crate::AgentError> {
        // Non-tracked callers (e.g. chat) do not share an exploration-
        // reset signal with a `TaskToolExecutor`; pass `None` so the
        // loop simply does not perform the reset.
        self.execute_task_inner(
            provider,
            executor,
            params,
            event_tx,
            cancel,
            TaskInnerOptions {
                phase_reset_signal: None,
                prebuilt_task_ctx: None,
                early_test_oracle: self.config.early_test_oracle,
            },
        )
        .await
    }

    async fn execute_task_inner(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        params: &AgenticTaskParams<'_>,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancel: Option<CancellationToken>,
        options: TaskInnerOptions,
    ) -> Result<TaskExecutionResult, crate::AgentError> {
        let TaskInnerOptions {
            phase_reset_signal,
            prebuilt_task_ctx,
            early_test_oracle,
        } = options;
        let complexity = classify_task_complexity(params.task.title, params.task.description);

        let test_command_override = aura_config::agent().verify.test_command_override.clone();
        let system_prompt = agentic_execution_system_prompt(
            params.project,
            params.agent,
            test_command_override.as_deref(),
        );

        // Reuse the caller-supplied full task context bundle when
        // present (the `execute_task_tracked` path pre-builds it so
        // the `get_task_context` tool can return a non-empty payload
        // — see `build_full_task_ctx`) and only build a fresh one for
        // the non-tracked path. Either way we cap the copy used as
        // the initial user message via `cap_bootstrap_task_context`
        // so model routing stays small; the full bundle (when
        // provided) is the executor's source of truth for the tool
        // response and stays untruncated.
        let mut task_ctx = match prebuilt_task_ctx {
            Some(ctx) => ctx,
            None => build_full_task_ctx(params).await,
        };
        task_context::cap_bootstrap_task_context(&mut task_ctx);

        let mut loop_config =
            configure_loop_config(complexity, &self.config, params.member_count, system_prompt);
        loop_config.phase_reset_signal = phase_reset_signal;
        // Phase 5: install the EarlyTestOracle source via the
        // per-call `early_test_oracle` flag (the runner-level
        // default from `AgentRunnerConfig.early_test_oracle` for
        // `execute_task` callers, or the per-task override from
        // `TaskTrackingConfig.early_test_oracle` for
        // `execute_task_tracked` callers). The oracle is permanently
        // disarmed when the project declares no `test_command`, so
        // we only attach the config when both `enabled` AND a
        // non-blank command are present.
        loop_config.early_test_oracle = if early_test_oracle {
            let test_command = params
                .project
                .test_command
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty());
            test_command.map(|cmd| aura_config::EarlyTestOracleConfig {
                enabled: true,
                test_command: Some(cmd),
            })
        } else {
            None
        };

        let agent_loop = AgentLoop::new(loop_config);
        let messages = vec![Message::user(&task_ctx)];

        let result = agent_loop
            .run_with_events(
                provider,
                executor,
                messages,
                params.tools.clone(),
                event_tx,
                cancel,
            )
            .await?;

        if let Some(ref llm_err) = result.llm_error {
            return Err(crate::AgentError::Internal(format!("LLM error: {llm_err}")));
        }
        if result.iterations == 0 {
            return Err(crate::AgentError::Internal(
                "Agent loop completed zero iterations — LLM may not be configured correctly".into(),
            ));
        }

        Ok(finalize_loop_result(result))
    }

    /// Execute an agentic task with built-in plan gating, file tracking,
    /// self-review, and stub detection.
    ///
    /// This is the preferred entry point for automatons: it internally
    /// constructs a [`TaskToolExecutor`] and merges its tracked state
    /// (file ops, notes, follow-ups, phase) into the returned result.
    pub async fn execute_task_tracked(
        &self,
        provider: &dyn ModelProvider,
        tracking: TaskTrackingConfig,
        params: &AgenticTaskParams<'_>,
        event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
        cancel: Option<CancellationToken>,
    ) -> Result<TaskExecutionResult, crate::AgentError> {
        // Shared `Arc<AtomicBool>` between the task executor and the
        // wrapped agent loop. Pre-seeded to `true` so the first
        // iteration's `LoopState::begin_iteration` zeroes exploration
        // and read-guard counters, bumps the allowance with the
        // implement-phase bonus, and arms the post-plan exploration
        // hard block — giving every task the fresh budget that used to
        // require a successful `submit_plan` call.
        // `handle_submit_plan` still flips this back to `true` when a
        // plan is accepted mid-run so an explicit re-plan also gets a
        // fresh budget. See `harness_task_completion_fix` Phase 1.
        let reset_signal = Arc::new(AtomicBool::new(true));
        // Build the full (uncapped) task-context bundle ONCE here so
        // both the `get_task_context` tool response and the initial
        // user message inside `execute_task_inner` derive from the
        // same source. Historically `task_context` was hard-coded to
        // `String::new()` on the executor, so every call to
        // `get_task_context` returned an empty payload and forced the
        // agent into a long `list_files`/`read_file`/`search_code`
        // burn just to rediscover the task description the server
        // already had — see the `submit_plan` deadlock investigation.
        // Phase 4 update: the helper is now async because attempt-0
        // contexts splice in a pre-resolved paths/symbols block (best
        // effort, 2s per-call timeout, falls back cleanly to the
        // pre-Phase-4 prompt on any IO failure). Still called ONCE so
        // both the `get_task_context` tool response and the initial
        // user message inside `execute_task_inner` derive from the
        // same source, and so we never pay the resolve cost twice.
        let full_task_ctx = build_full_task_ctx(params).await;
        // Phase 5: per-task override on top of the runner-level
        // default. Captured before the `tracking` struct is moved
        // into the executor below.
        let early_test_oracle = tracking
            .early_test_oracle
            .unwrap_or(self.config.early_test_oracle);
        let task_executor = TaskToolExecutor {
            inner: tracking.inner_executor,
            project_folder: tracking.project_folder,
            build_command: tracking.build_command,
            test_command: tracking.test_command,
            test_command_override: crate::task_executor::read_test_command_override_env(),
            task_context: full_task_ctx.clone(),
            tracked_file_ops: Arc::default(),
            notes: Arc::default(),
            follow_ups: Arc::default(),
            stub_fix_attempts: Arc::default(),
            test_runner: Arc::new(crate::task_executor::RealTaskTestRunner),
            task_phase: Arc::new(Mutex::new(TaskPhase::Implementing {
                plan: TaskPlan::empty(),
            })),
            self_review: Arc::default(),
            event_tx: event_tx.clone(),
            no_changes_needed: Arc::default(),
            recent_tool_outcomes: Arc::default(),
            reset_explore_on_phase_change: Arc::clone(&reset_signal),
        };

        let mut result = self
            .execute_task_inner(
                provider,
                &task_executor,
                params,
                event_tx,
                cancel,
                TaskInnerOptions {
                    phase_reset_signal: Some(reset_signal),
                    prebuilt_task_ctx: Some(full_task_ctx),
                    early_test_oracle,
                },
            )
            .await?;

        task_executor.merge_into_result(&mut result).await;
        Ok(result)
    }

    /// Execute a chat interaction using the agent loop.
    pub async fn execute_chat(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        prompt: ChatPromptCtx<'_>,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        hooks: ChatHooks,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        let ChatHooks { event_tx, cancel } = hooks;
        let ChatPromptCtx {
            project,
            custom_system_prompt,
        } = prompt;
        let system_prompt = {
            let project_id = project.project_id.map(str::to_owned);
            let name = project.name.to_owned();
            let description = project.description.to_owned();
            let folder_path = project.folder_path.to_owned();
            let build_command = project.build_command.map(str::to_owned);
            let test_command = project.test_command.map(str::to_owned);
            let custom = custom_system_prompt.to_owned();
            tokio::task::spawn_blocking(move || {
                let p = ProjectInfo {
                    project_id: project_id.as_deref(),
                    name: &name,
                    description: &description,
                    folder_path: &folder_path,
                    build_command: build_command.as_deref(),
                    test_command: test_command.as_deref(),
                };
                build_chat_system_prompt(&p, &custom, None)
            })
            .await?
        };
        let config = AgentLoopConfig {
            system_prompt,
            max_tokens: self.config.task_execution_max_tokens,
            stream_timeout: Duration::from_secs(self.config.stream_timeout_secs),
            billing_reason: "aura_chat".to_string(),
            max_context_tokens: Some(self.config.max_context_tokens),
            // Mirror `configure_loop_config`: when an automaton's
            // built-in chat (e.g. the dev-loop's planner chat) goes
            // through this path, propagate the same router/billing
            // identifiers so the outbound headers match the
            // interactive WebSocket chat path.
            aura_org_id: self.config.aura_org_id.clone(),
            aura_session_id: self.config.aura_session_id.clone(),
            aura_agent_id: self.config.aura_agent_id.clone(),
            aura_project_id: self.config.aura_project_id.clone(),
            prompt_cache_key: self.config.prompt_cache_key.clone(),
            ..AgentLoopConfig::for_agent(self.config.default_model.clone())
        };
        let agent_loop = AgentLoop::new(config);
        agent_loop
            .run_with_events(provider, executor, messages, tools, event_tx, cancel)
            .await
    }

    /// Execute a shell task with automatic retry on failure.
    pub async fn execute_shell_task(
        &self,
        params: &ShellTaskParams<'_>,
        event_tx: Option<&mpsc::Sender<AgentLoopEvent>>,
    ) -> Result<TaskExecutionResult, crate::AgentError> {
        let command = auto_correct_build_command(params.command)
            .unwrap_or_else(|| params.command.to_string());
        let max_attempts = self.config.max_shell_task_retries;
        let mut prior: Vec<BuildFixAttemptRecord> = Vec::new();

        for attempt in 1..=max_attempts {
            if let Some(tx) = event_tx {
                let _ = tx.try_send(AgentLoopEvent::TextDelta(format!(
                    "Running: {command} (attempt {attempt}/{max_attempts})\n",
                )));
            }

            let result = run_build_command(params.project_root, &command, None)
                .await
                .map_err(|e| crate::AgentError::BuildFailed(e.to_string()))?;

            if result.success {
                let notes = format!(
                    "Command `{}` succeeded on attempt {attempt}.\n{}",
                    command, result.stdout,
                );
                if let Some(tx) = event_tx {
                    let _ = tx.try_send(AgentLoopEvent::TextDelta(notes.clone()));
                }
                return Ok(TaskExecutionResult {
                    notes,
                    files_already_applied: false,
                    ..TaskExecutionResult::default()
                });
            }

            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };

            if let Some(tx) = event_tx {
                let _ = tx.try_send(AgentLoopEvent::TextDelta(format!(
                    "Command failed (attempt {attempt}):\n{detail}\n",
                )));
            }

            if let Some(err) = check_repeated_error(
                &prior,
                &normalize_error_signature(detail),
                attempt,
                &command,
            ) {
                return Err(crate::AgentError::BuildFailed(err.to_string()));
            }

            if attempt < max_attempts {
                prior.push(BuildFixAttemptRecord {
                    stderr: detail.clone(),
                    error_signature: normalize_error_signature(detail),
                    files_changed: Vec::new(),
                    changes_summary: String::new(),
                });
            }
        }

        Err(crate::AgentError::BuildFailed(format!(
            "command `{command}` failed after {max_attempts} attempts"
        )))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an [`AgentLoopConfig`] from task complexity and runner config.
#[must_use]
pub fn configure_loop_config(
    complexity: TaskComplexity,
    config: &AgentRunnerConfig,
    _member_count: usize,
    system_prompt: String,
) -> AgentLoopConfig {
    // Stripped (2026-05): the per-complexity budget escalation
    // (Standard scaled by `_member_count`, Complex floored to 12_000)
    // amplified per-turn deliberation without translating into faster
    // convergence. Hold every task at the configured base — Simple
    // still gets the same cap as before so trivial tasks don't burn
    // tokens, but Standard and Complex inherit the runner's
    // `thinking_budget` floor.
    let thinking_budget = match complexity {
        TaskComplexity::Simple => 2_000.min(config.thinking_budget),
        TaskComplexity::Standard | TaskComplexity::Complex => config.thinking_budget,
    };
    let max_tokens = match complexity {
        TaskComplexity::Simple => config.task_execution_max_tokens.min(8_192),
        TaskComplexity::Complex => config.task_execution_max_tokens.max(32_768),
        TaskComplexity::Standard => config.task_execution_max_tokens,
    };
    let max_iterations = match complexity {
        TaskComplexity::Simple => config.max_agentic_iterations.min(15),
        _ => config.max_agentic_iterations,
    };
    let model = match complexity {
        TaskComplexity::Simple => resolve_simple_model(&config.simple_model),
        _ => config.default_model.clone(),
    };

    // Phase 6: the policy-derived `thinking_budget` is the *starting*
    // per-iteration response budget for `LoopState::thinking.budget`.
    // The agent loop tapers it across iterations and the on-truncation
    // recovery path lifts back to `max_tokens`. We forward it via
    // `AgentLoopConfig::thinking_budget` (capped at `max_tokens` so the
    // ceiling invariant in `LoopState::build_request` still holds).
    let initial_thinking_budget = thinking_budget.min(max_tokens);

    // Phase 5: the `early_test_oracle` field on the returned
    // `AgentLoopConfig` is populated later inside `execute_task_inner`
    // because that's where `params.project.test_command` is in
    // scope. The struct field defaults to `None` here.
    AgentLoopConfig {
        max_iterations,
        max_tokens,
        thinking_budget: Some(initial_thinking_budget),
        stream_timeout: Duration::from_secs(config.stream_timeout_secs),
        billing_reason: "aura_task".to_string(),
        max_context_tokens: Some(config.max_context_tokens),
        credit_budget: config.max_task_credits,
        auto_build_cooldown: 1,
        auth_token: config.auth_token.clone(),
        system_prompt,
        // Forward the router/billing identifiers populated by the
        // automaton bridge so outbound `/v1/messages` calls carry
        // `X-Aura-Org-Id` / `X-Aura-Session-Id`. Same pattern as the
        // interactive-chat path (`SessionState`) — the loop builder
        // at line ~676 of `agent_loop/mod.rs` calls
        // `.aura_org_id(config.aura_org_id.clone())` /
        // `.aura_session_id(...)` on the `ModelRequest`, so we just
        // need to make sure the `AgentLoopConfig` we hand it is
        // populated.
        aura_org_id: config.aura_org_id.clone(),
        aura_session_id: config.aura_session_id.clone(),
        aura_agent_id: config.aura_agent_id.clone(),
        aura_project_id: config.aura_project_id.clone(),
        prompt_cache_key: config.prompt_cache_key.clone(),
        request_kind: ModelRequestKind::DevLoopBootstrap,
        // Temporary (2026-05): the dev-loop policy pins reasoning
        // effort to `Medium` across every iteration. This flag is
        // the "this is a dev-loop run" signal that
        // `LoopState::compute_thinking_effort` reads to bypass the
        // codex-style `Off → Medium → Low` taper still used by chat
        // and other generic callers. The historical iteration-0
        // `max_tokens` clamp that used to ride on this flag has been
        // removed (see `AgentLoopConfig::disable_thinking_iteration_0`).
        disable_thinking_iteration_0: true,
        ..AgentLoopConfig::for_agent(model)
    }
}

/// Build the task-context bundle that backs both the initial user
/// message and the `get_task_context` tool response.
///
/// Stripped (2026-05): historically this returned the full
/// project/spec/task header plus a 160 KB bundle of workspace map,
/// type defs, codebase snapshot, and dep-API surface (capped via
/// `task_context::MAX_TASK_CONTEXT_CHARS`). On every `get_task_context`
/// call the agent received ~40 K tokens of pre-bundled material; once
/// compaction redacted the older copy the model called the tool again
/// and the conversation thrashed. The agent already has
/// `read_file` / `search_code` / `list_files` — give it the task text
/// and let it pull the rest on demand.
///
/// On `params.attempt == 0` this resolves the Phase 4 enrichment block
/// (paths + symbols extracted from the task description, looked up on
/// disk) and splices it into the returned context. Each per-call IO is
/// wrapped in a 2-second timeout (see [`default_caps`]), so a slow
/// filesystem or grep can NEVER block context construction — the worst
/// case is that the enrichment block is omitted and the agent gets the
/// pre-Phase-4 prompt. On retries the resolve is skipped entirely.
async fn build_full_task_ctx(params: &AgenticTaskParams<'_>) -> String {
    let work_log_summary = task_context::build_work_log_summary(params.work_log);
    let enrichment_block = resolve_enrichment_block(params).await;
    build_agentic_task_context(
        params.project,
        params.spec,
        params.task,
        params.session,
        params.completed_deps,
        &work_log_summary,
        params.attempt,
        enrichment_block.as_deref(),
    )
}

/// Phase 4: extract candidate paths & symbols from the task description
/// and resolve them against the project folder. Returns `None` (and
/// emits nothing in the task context) when:
/// - `attempt > 0` (retries skip enrichment — see Phase 5),
/// - the description has no resolvable hints,
/// - the project `folder_path` is empty / nonexistent,
/// - or every resolve call timed out / hit a missing file.
async fn resolve_enrichment_block(params: &AgenticTaskParams<'_>) -> Option<String> {
    if params.attempt != 0 {
        return None;
    }
    let text = format!("{} {}", params.task.title, params.task.description);
    let hints = extract_hints(&text);
    if !hints.is_meaningful() {
        return None;
    }
    let folder = params.project.folder_path;
    if folder.is_empty() {
        return None;
    }
    let workspace = FsWorkspace::new(folder);
    let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
    if resolved.is_empty() {
        return None;
    }
    Some(resolved.into_block())
}

/// Process an [`AgentLoopResult`] into a [`TaskExecutionResult`].
fn finalize_loop_result(result: AgentLoopResult) -> TaskExecutionResult {
    let AgentLoopResult {
        total_text,
        total_input_tokens,
        total_output_tokens,
        messages,
        ..
    } = result;
    let notes = if total_text.is_empty() {
        "Task completed via agentic tool-use loop".to_string()
    } else {
        total_text
    };
    TaskExecutionResult {
        notes,
        follow_up_tasks: Vec::new(),
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        files_already_applied: true,
        messages,
    }
}

/// Check if the same error signature is repeating across fix attempts.
///
/// Returns an error if the same pattern has appeared 3+ consecutive times.
pub fn check_repeated_error(
    prior: &[BuildFixAttemptRecord],
    current_sig: &str,
    attempt: u32,
    command: &str,
) -> Option<anyhow::Error> {
    let consecutive_dupes = prior
        .iter()
        .rev()
        .take_while(|a| a.error_signature == current_sig)
        .count();
    if consecutive_dupes >= 2 {
        tracing::info!(
            attempt,
            "same shell error pattern repeated {} times, aborting fix loop",
            consecutive_dupes + 1,
        );
        return Some(anyhow::anyhow!(
            "command `{command}` keeps failing with the same error after {} attempts",
            consecutive_dupes + 1,
        ));
    }
    None
}

#[cfg(test)]
mod tests;
