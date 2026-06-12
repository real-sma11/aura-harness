//! Dev-loop control tools (`start_dev_loop`, `pause_dev_loop`, `stop_dev_loop`,
//! `run_task`).
//!
//! These tools let the chat agent manage automaton lifecycle within the harness.
//! Actual automaton operations are delegated to an [`AutomatonController`] trait
//! whose concrete implementation lives in the node layer (avoiding circular deps
//! with `aura-automaton`).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use aura_core_types::ToolDefinition;
use aura_core_types::ToolResult;

use crate::error::ToolError;
use crate::tool::{Tool, ToolContext};

// ---------------------------------------------------------------------------
// Controller trait (implemented in aura-runtime)
// ---------------------------------------------------------------------------

/// Abstraction over automaton lifecycle so tools don't depend on `aura-automaton`.
#[async_trait]
pub trait AutomatonController: Send + Sync {
    /// Install and start a dev-loop automaton for `project_id`.
    /// Returns the automaton ID on success.
    async fn start_dev_loop(
        &self,
        project_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
    ) -> Result<String, String>;

    /// Pause the running dev-loop for `project_id`.
    async fn pause_dev_loop(&self, project_id: &str) -> Result<(), String>;

    /// Stop (cancel) the running dev-loop for `project_id`.
    async fn stop_dev_loop(&self, project_id: &str) -> Result<(), String>;

    /// Execute a single task through the dev-loop engine (non-blocking).
    /// Returns the automaton ID immediately.
    #[allow(clippy::too_many_arguments)]
    async fn run_task(
        &self,
        project_id: &str,
        task_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
    ) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// start_dev_loop
// ---------------------------------------------------------------------------

pub struct StartDevLoopTool {
    controller: Arc<dyn AutomatonController>,
    project_id: String,
    workspace_root: Option<PathBuf>,
    auth_token: Option<String>,
}

impl StartDevLoopTool {
    pub fn new(
        controller: Arc<dyn AutomatonController>,
        project_id: String,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            controller,
            project_id,
            workspace_root,
            auth_token,
        }
    }
}

#[async_trait]
impl Tool for StartDevLoopTool {
    fn name(&self) -> &str {
        "start_dev_loop"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "start_dev_loop".into(),
            description: "Start the autonomous dev loop for the project. It will pick up ready tasks and execute them.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "model": { "type": "string", "description": "Optional model override for the loop (e.g. 'claude-sonnet-4-20250514')" }
                },
                "required": []
            }),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        // Fall back to the caller agent's resolved model when the tool
        // call omits `model`. The harness rejects a model-less dev-loop
        // start, and the chat agent (which has a model) is the one
        // invoking this — so thread its model through rather than
        // forcing the operator to pass it explicitly.
        let model = resolve_tool_model(&args, ctx);

        match self
            .controller
            .start_dev_loop(
                &self.project_id,
                self.workspace_root.clone(),
                self.auth_token.clone(),
                model,
                None,
                None,
            )
            .await
        {
            Ok(automaton_id) => Ok(ToolResult::success(
                "start_dev_loop",
                format!("Dev loop started (run_id: {automaton_id}). Monitor progress via /stream/{automaton_id}"),
            )),
            Err(e) => Ok(ToolResult::failure("start_dev_loop", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// pause_dev_loop
// ---------------------------------------------------------------------------

pub struct PauseDevLoopTool {
    controller: Arc<dyn AutomatonController>,
    project_id: String,
}

impl PauseDevLoopTool {
    pub fn new(controller: Arc<dyn AutomatonController>, project_id: String) -> Self {
        Self {
            controller,
            project_id,
        }
    }
}

#[async_trait]
impl Tool for PauseDevLoopTool {
    fn name(&self) -> &str {
        "pause_dev_loop"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "pause_dev_loop".into(),
            description: "Pause the currently running dev loop.".into(),
            input_schema: serde_json::json!({"type":"object","properties":{},"required":[]}),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        match self.controller.pause_dev_loop(&self.project_id).await {
            Ok(()) => Ok(ToolResult::success("pause_dev_loop", "Dev loop paused")),
            Err(e) => Ok(ToolResult::failure("pause_dev_loop", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// stop_dev_loop
// ---------------------------------------------------------------------------

pub struct StopDevLoopTool {
    controller: Arc<dyn AutomatonController>,
    project_id: String,
}

impl StopDevLoopTool {
    pub fn new(controller: Arc<dyn AutomatonController>, project_id: String) -> Self {
        Self {
            controller,
            project_id,
        }
    }
}

#[async_trait]
impl Tool for StopDevLoopTool {
    fn name(&self) -> &str {
        "stop_dev_loop"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "stop_dev_loop".into(),
            description: "Stop the currently running dev loop.".into(),
            input_schema: serde_json::json!({"type":"object","properties":{},"required":[]}),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        match self.controller.stop_dev_loop(&self.project_id).await {
            Ok(()) => Ok(ToolResult::success("stop_dev_loop", "Dev loop stopped")),
            Err(e) => Ok(ToolResult::failure("stop_dev_loop", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// run_task
// ---------------------------------------------------------------------------

pub struct RunTaskTool {
    controller: Arc<dyn AutomatonController>,
    project_id: String,
    workspace_root: Option<PathBuf>,
    auth_token: Option<String>,
}

impl RunTaskTool {
    pub fn new(
        controller: Arc<dyn AutomatonController>,
        project_id: String,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            controller,
            project_id,
            workspace_root,
            auth_token,
        }
    }
}

#[async_trait]
impl Tool for RunTaskTool {
    fn name(&self) -> &str {
        "run_task"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_task".into(),
            description: "Start execution of a single task by the dev-loop engine. Returns immediately; monitor progress via the automaton event stream.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "model": { "type": "string", "description": "Optional model override" }
                },
                "required": ["task_id"]
            }),
            cache_control: None,
            eager_input_streaming: None,
        }
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'task_id' argument".into()))?;
        let model = resolve_tool_model(&args, ctx);

        match self
            .controller
            .run_task(
                &self.project_id,
                task_id,
                self.workspace_root.clone(),
                self.auth_token.clone(),
                model,
                None,
                None,
            )
            .await
        {
            Ok(automaton_id) => Ok(ToolResult::success(
                "run_task",
                format!("Task execution started (run_id: {automaton_id}). Monitor via /stream/{automaton_id}"),
            )),
            Err(e) => Ok(ToolResult::failure("run_task", e)),
        }
    }
}

/// Resolve the model for a dev-loop / task-run tool call: an explicit,
/// non-blank `model` arg wins; otherwise fall back to the caller
/// agent's resolved model (`ToolContext::caller_model_id`). The harness
/// rejects a model-less start request, and the invoking chat agent
/// already has a model, so threading it through here lets the agent
/// start the loop without re-specifying its own model.
fn resolve_tool_model(args: &serde_json::Value, ctx: &ToolContext) -> Option<String> {
    args.get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            ctx.caller_model_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

/// Create all dev-loop control tools for a session context.
pub fn devloop_control_tools(
    controller: Arc<dyn AutomatonController>,
    project_id: String,
    workspace_root: Option<PathBuf>,
    auth_token: Option<String>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(StartDevLoopTool::new(
            controller.clone(),
            project_id.clone(),
            workspace_root.clone(),
            auth_token.clone(),
        )),
        Box::new(PauseDevLoopTool::new(
            controller.clone(),
            project_id.clone(),
        )),
        Box::new(StopDevLoopTool::new(controller.clone(), project_id.clone())),
        Box::new(RunTaskTool::new(
            controller,
            project_id,
            workspace_root,
            auth_token,
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned set of dev-loop tool names the harness registers via
    /// [`devloop_control_tools`]. These names must stay aligned with
    /// `ToolCatalog` and aura-os-server's native-tool dedupe list so
    /// clients validate and display the same automaton control surface
    /// that the resolver can execute.
    const CANONICAL_DEVLOOP_TOOL_NAMES: &[&str] = &[
        "start_dev_loop",
        "pause_dev_loop",
        "stop_dev_loop",
        "run_task",
    ];

    struct StubController;

    #[async_trait]
    impl AutomatonController for StubController {
        async fn start_dev_loop(
            &self,
            _project_id: &str,
            _workspace_root: Option<PathBuf>,
            _auth_token: Option<String>,
            _model: Option<String>,
            _git_repo_url: Option<String>,
            _git_branch: Option<String>,
        ) -> Result<String, String> {
            Ok("stub".into())
        }
        async fn pause_dev_loop(&self, _project_id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn stop_dev_loop(&self, _project_id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn run_task(
            &self,
            _project_id: &str,
            _task_id: &str,
            _workspace_root: Option<PathBuf>,
            _auth_token: Option<String>,
            _model: Option<String>,
            _git_repo_url: Option<String>,
            _git_branch: Option<String>,
        ) -> Result<String, String> {
            Ok("stub".into())
        }
    }

    #[test]
    fn devloop_control_tools_returns_canonical_name_set() {
        let controller: Arc<dyn AutomatonController> = Arc::new(StubController);
        let tools = devloop_control_tools(controller, "proj-1".into(), None, None);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, CANONICAL_DEVLOOP_TOOL_NAMES);
    }

    fn ctx_with_caller_model(model: Option<&str>) -> ToolContext {
        let mut ctx =
            ToolContext::new(crate::sandbox::Sandbox::new(&std::env::temp_dir()).unwrap(), crate::ToolConfig::default());
        ctx.caller_model_id = model.map(String::from);
        ctx
    }

    #[test]
    fn resolve_tool_model_prefers_explicit_arg() {
        let ctx = ctx_with_caller_model(Some("caller-model"));
        let args = serde_json::json!({ "model": "explicit-model" });
        assert_eq!(
            resolve_tool_model(&args, &ctx).as_deref(),
            Some("explicit-model")
        );
    }

    #[test]
    fn resolve_tool_model_falls_back_to_caller_model() {
        let ctx = ctx_with_caller_model(Some("caller-model"));
        let args = serde_json::json!({});
        assert_eq!(
            resolve_tool_model(&args, &ctx).as_deref(),
            Some("caller-model"),
            "a model-less start must inherit the calling agent's model"
        );
    }

    #[test]
    fn resolve_tool_model_treats_blank_values_as_unset() {
        let ctx = ctx_with_caller_model(Some("   "));
        let args = serde_json::json!({ "model": "  " });
        assert_eq!(resolve_tool_model(&args, &ctx), None);
    }
}
