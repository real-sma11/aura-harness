//! Shared `ModelRequest::builder` template for the auxiliary
//! single-shot LLM calls used by [`super::super::spec_gen`] and
//! [`super::super::task_refinement`].
//!
//! Both call sites previously hand-built a `ModelRequest` with
//! slightly different (and easy-to-drift) `max_tokens` defaults and
//! tool-policy settings. This helper centralizes the build so the
//! auxiliary-call shape only changes in one place and so the
//! `max_tokens` ceiling pulls cleanly from
//! `aura_config::agent().automaton.*` instead of crate-local
//! `pub const`s.
//!
//! Streaming, mid-stream cancellation, and the `early_test_oracle`
//! steering path do not apply here — these are one-shot
//! synchronous-shape calls whose result populates a config string
//! (spec body / refined task description) rather than driving a tool
//! loop. The helper therefore deliberately exposes nothing more than
//! `system_prompt`, `user_body`, `max_tokens`, and the model
//! identifier.

use aura_reasoner::{Message, ModelProvider, ModelRequest, ModelResponse, ToolChoice};

use crate::error::AutomatonError;

/// Inputs for a single auxiliary LLM call.
///
/// Callers populate this struct from their own per-tick state (the
/// validated model identifier, the task or spec body, etc.); the
/// helper handles the `ModelRequest::builder` template and the
/// `AutomatonError` wrap on either the builder validation step or the
/// provider rejection step.
pub(crate) struct AuxiliaryModelCall<'a> {
    pub(crate) model: &'a str,
    pub(crate) system_prompt: &'a str,
    pub(crate) user_body: String,
    pub(crate) max_tokens: u32,
    /// Optional task scope forwarded into any
    /// [`AutomatonError::AgentExecution`] surfaced from this call.
    /// `None` for project-scoped calls (spec generation); `Some(task_id)`
    /// for task-scoped calls (refinement).
    pub(crate) task_scope: Option<String>,
}

/// Build and dispatch the auxiliary call. Translates `ReasonerError`
/// into the typed [`AutomatonError::AgentExecution`] variant via
/// [`aura_agent::AgentError::Reason`] so callers stay terse.
pub(crate) async fn run_auxiliary_model_call(
    provider: &dyn ModelProvider,
    call: AuxiliaryModelCall<'_>,
) -> Result<ModelResponse, AutomatonError> {
    let AuxiliaryModelCall {
        model,
        system_prompt,
        user_body,
        max_tokens,
        task_scope,
    } = call;

    let request = ModelRequest::builder(model, system_prompt)
        .messages(vec![Message::user(user_body)])
        .tools(Vec::new())
        .tool_choice(ToolChoice::None)
        .max_tokens(max_tokens)
        .try_build()
        .map_err(|e| {
            AutomatonError::agent_execution(task_scope.clone(), aura_agent::AgentError::Reason(e))
        })?;

    provider
        .complete(request)
        .await
        .map_err(|e| AutomatonError::agent_execution(task_scope, aura_agent::AgentError::Reason(e)))
}
