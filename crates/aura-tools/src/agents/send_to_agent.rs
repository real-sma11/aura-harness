//! `send_to_agent` — phase 5 cross-agent tool.
//!
//! Delivers a user-message-shaped payload to a target agent. The caller must
//! hold [`Capability::ControlAgent`] and the target must be in the caller's
//! [`AgentScope::agent_ids`] (universe scope = any target allowed).
//!
//! Runtime effect (message delivery to the target agent's reasoner) is
//! performed by [`crate::AgentControlHook::deliver_message`] when wired.
//! Without a hook the tool still executes the gate and returns a
//! descriptive outcome — see [`crate::agents`] module docs.
//!
//! ## Reply delivery contract
//!
//! `send_to_agent` is intentionally non-blocking: the tool returns as soon
//! as the target's `user_message` is persisted (the `x-aura-chat-persisted`
//! header on the SSE response is `true`). The target's reply is delivered
//! **asynchronously**: when its `AssistantMessageEnd` lands, the
//! aura-os-server persist task posts a follow-up `user_message` into the
//! originating agent's session carrying the target's reply text. The LLM
//! then sees that follow-up as a fresh turn and can react to it.
//!
//! The successful `ToolResult` carries a `reply_delivery=async_user_message`
//! metadata tag so the LLM-side prompt copy can stop trying to read the
//! reply out of the synchronous `delivered: true` body. See
//! `apps/aura-os-server/src/handlers/agents/chat/persist_task.rs` in the
//! aura-os repo for the server-side `spawn_cross_agent_reply_callback`
//! that owns the post-back side of this contract.

use crate::error::ToolError;
use crate::tool::{Tool, ToolContext};
use async_trait::async_trait;
use aura_core_types::{Capability, ToolDefinition, ToolResult};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const SEND_TO_AGENT_TOOL_NAME: &str = "send_to_agent";

/// Input schema for [`SendToAgentTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendToAgentInput {
    /// Target agent id (hex).
    pub agent_id: String,
    /// Message content to deliver.
    pub content: String,
    /// Optional structured attachments to forward along with the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendToAgentOutcome {
    pub target_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_id: Option<String>,
    pub delivered: bool,
    /// Human-readable guidance surfaced to the calling LLM in the tool
    /// result body. Set on a successful delivery to spell out the
    /// async-reply contract so the model waits for the target's reply
    /// to arrive on its own instead of trying to poll the target's
    /// state. Elided (and `None`) on the permission-gate-only
    /// [`SendToAgentTool::evaluate`] path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Guidance string stamped onto a successful [`SendToAgentOutcome`].
/// Kept as a constant so the wording stays consistent across the tool
/// body and is greppable from tests.
pub(crate) const ASYNC_REPLY_NOTE: &str =
    "Delivered. The target agent's reply will be delivered back to you automatically \
     as a new message in this conversation once it finishes responding. Do NOT poll, \
     and do NOT call get_agent_state to fetch the reply — simply end your turn. You \
     will be re-invoked with the reply when it is ready, and can relay it then.";

pub struct SendToAgentTool;

pub(crate) fn missing_runtime_hook(tool_name: &str) -> ToolResult {
    ToolResult::failure(
        tool_name,
        format!("{tool_name}: runtime hook not wired; configure AURA_OS_SERVER_URL on aura-node"),
    )
}

impl SendToAgentTool {
    #[must_use]
    pub fn definition() -> ToolDefinition {
        ToolDefinition::new(
            SEND_TO_AGENT_TOOL_NAME,
            "Send a message to another agent within the caller's scope. \
             Requires Capability::ControlAgent. Delivery is asynchronous: \
             the call returns once the message is delivered, and the target \
             agent's reply is delivered back to you automatically as a new \
             message in this conversation when it finishes. Do not poll the \
             target or call get_agent_state to retrieve the reply — just end \
             your turn; you will be re-invoked with the reply when it is ready.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "content": { "type": "string" },
                    "attachments": {}
                },
                "required": ["agent_id", "content"]
            }),
        )
    }

    /// Pure gate — evaluates the permission check without performing any
    /// runtime side-effect.
    pub fn evaluate(
        ctx: &ToolContext,
        input: &SendToAgentInput,
    ) -> Result<SendToAgentOutcome, ToolError> {
        evaluate_control_gate(ctx, &input.agent_id, "send_to_agent")?;
        Ok(SendToAgentOutcome {
            target_agent_id: input.agent_id.clone(),
            parent_agent_id: caller_external_id(ctx),
            originating_user_id: ctx.originating_user_id.clone(),
            delivered: false,
            note: None,
        })
    }
}

/// Resolve the caller's id for cross-agent server callbacks.
///
/// Always prefer the upstream OS UUID (`ctx.caller_external_agent_id`)
/// because it is what `aura-os-server`'s
/// `Path<AgentId = Uuid>` extractor on
/// `/api/agents/{originating_agent_id}/events/stream` expects when the
/// server-side `spawn_cross_agent_reply_callback` posts the recipient's
/// reply back into the originator's session. Falling back to
/// `ctx.caller_agent_id.to_string()` only preserves test scaffolding
/// that constructs `ToolContext` directly without the runtime
/// `with_caller_external_agent_id` wiring; in production the runtime
/// always populates the external id from `SessionState::skill_agent_id`,
/// so the fallback never fires for live chat. See the field doc on
/// `ToolContext::caller_external_agent_id` for the cross-repo
/// rationale.
fn caller_external_id(ctx: &ToolContext) -> Option<String> {
    if let Some(external) = ctx
        .caller_external_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return Some(external.to_string());
    }
    ctx.caller_agent_id.map(|id| id.to_string())
}

#[async_trait]
impl Tool for SendToAgentTool {
    fn name(&self) -> &str {
        SEND_TO_AGENT_TOOL_NAME
    }

    fn definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn required_capabilities(&self) -> Vec<Capability> {
        vec![Capability::ControlAgent]
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let input: SendToAgentInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("send_to_agent: {e}")))?;

        let mut outcome = match Self::evaluate(ctx, &input) {
            Ok(o) => o,
            Err(err) => {
                return Ok(ToolResult::failure(
                    SEND_TO_AGENT_TOOL_NAME,
                    Bytes::from(err.to_string().into_bytes()),
                ));
            }
        };

        let Some(hook) = ctx.agent_control_hook.as_ref() else {
            return Ok(missing_runtime_hook(SEND_TO_AGENT_TOOL_NAME));
        };

        let parent = caller_external_id(ctx);
        match hook
            .deliver_message(
                &input.agent_id,
                parent.as_deref(),
                ctx.originating_user_id.as_deref(),
                ctx.caller_project_id.as_deref(),
                &input.content,
                input.attachments.clone(),
                ctx.caller_model_id.as_deref(),
            )
            .await
        {
            Ok(()) => {
                outcome.delivered = true;
                outcome.note = Some(ASYNC_REPLY_NOTE.to_string());
            }
            Err(err) => {
                return Ok(ToolResult::failure(
                    SEND_TO_AGENT_TOOL_NAME,
                    Bytes::from(format!("send_to_agent hook: {err}").into_bytes()),
                ));
            }
        }

        let body = serde_json::to_vec(&outcome)
            .map_err(|e| ToolError::Serialization(format!("send_to_agent outcome: {e}")))?;
        // `reply_delivery=async_user_message` documents the cross-repo
        // contract: aura-os-server posts the target's reply back into
        // the caller's session as a new `user_message` once the target's
        // turn finishes (see `spawn_cross_agent_reply_callback` in
        // `apps/aura-os-server/src/handlers/agents/chat/persist_task.rs`).
        // The LLM-side prompt copy can read this hint and stop trying
        // to read the reply out of the synchronous ToolResult body.
        Ok(ToolResult::success(SEND_TO_AGENT_TOOL_NAME, body)
            .with_metadata("target_agent_id", outcome.target_agent_id.clone())
            .with_metadata("reply_delivery", "async_user_message"))
    }
}

// ---------------------------------------------------------------------------
// Shared permission gate helpers (used by send_to_agent / agent_lifecycle /
// delegate_task / get_agent_state).
// ---------------------------------------------------------------------------

pub(crate) fn evaluate_control_gate(
    ctx: &ToolContext,
    target_agent_id: &str,
    tool_name: &str,
) -> Result<(), ToolError> {
    evaluate_gate(ctx, target_agent_id, tool_name, &Capability::ControlAgent)
}

pub(crate) fn evaluate_read_gate(
    ctx: &ToolContext,
    target_agent_id: &str,
    tool_name: &str,
) -> Result<(), ToolError> {
    evaluate_gate(ctx, target_agent_id, tool_name, &Capability::ReadAgent)
}

fn evaluate_gate(
    ctx: &ToolContext,
    target_agent_id: &str,
    tool_name: &str,
    required: &Capability,
) -> Result<(), ToolError> {
    let caller_permissions = ctx.caller_permissions.as_ref().ok_or_else(|| {
        ToolError::InvalidArguments(format!(
            "{tool_name} requires caller_permissions on the tool context"
        ))
    })?;

    if !caller_permissions.capabilities.contains(required) {
        return Err(ToolError::InvalidArguments(format!(
            "permissions: {tool_name} requires {required:?} capability"
        )));
    }

    let scope = &caller_permissions.scope;
    if !scope.agent_ids.is_empty() && !scope.agent_ids.iter().any(|id| id == target_agent_id) {
        return Err(ToolError::InvalidArguments(format!(
            "permissions: target agent '{target_agent_id}' is not within the caller's AgentScope::agent_ids"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Sandbox;
    use crate::ToolConfig;
    use async_trait::async_trait;
    use aura_core_types::{AgentId, AgentPermissions, AgentScope};
    use std::sync::Arc;

    fn ctx(caller: AgentPermissions) -> ToolContext {
        let dir = std::env::temp_dir();
        let mut ctx = ToolContext::new(Sandbox::new(&dir).unwrap(), ToolConfig::default());
        ctx.caller_permissions = Some(caller);
        ctx.caller_agent_id = Some(AgentId::generate());
        ctx.originating_user_id = Some("user-root".into());
        ctx
    }

    /// In-memory `AgentControlHook` used to drive the `execute` path
    /// without standing up a real aura-os-server. `deliver_message`
    /// returns `Ok(())` so the success branch is exercised.
    struct OkHook;

    #[async_trait]
    impl crate::tool::AgentControlHook for OkHook {
        async fn deliver_message(
            &self,
            _target_agent_id: &str,
            _parent_agent_id: Option<&str>,
            _originating_user_id: Option<&str>,
            _project_id: Option<&str>,
            _content: &str,
            _attachments: Option<serde_json::Value>,
            _model: Option<&str>,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn lifecycle(
            &self,
            _target_agent_id: &str,
            _parent_agent_id: Option<&str>,
            _originating_user_id: Option<&str>,
            _action: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn delegate_task(
            &self,
            _target_agent_id: &str,
            _parent_agent_id: Option<&str>,
            _originating_user_id: Option<&str>,
            _task: &str,
            _context: Option<&serde_json::Value>,
            _model: Option<&str>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn send_to_agent_requires_control_capability() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ReadAgent],
        };
        let input = SendToAgentInput {
            agent_id: "aa".into(),
            content: "hello".into(),
            attachments: None,
        };
        let err = SendToAgentTool::evaluate(&ctx(caller), &input).unwrap_err();
        assert!(err.to_string().contains("permissions:"), "got: {err}");
        assert!(err.to_string().contains("ControlAgent"), "got: {err}");
    }

    #[test]
    fn send_to_agent_denies_out_of_scope_target() {
        let caller = AgentPermissions {
            scope: AgentScope {
                agent_ids: vec!["allowed".into()],
                ..AgentScope::default()
            },
            capabilities: vec![Capability::ControlAgent],
        };
        let input = SendToAgentInput {
            agent_id: "not-allowed".into(),
            content: "hello".into(),
            attachments: None,
        };
        let err = SendToAgentTool::evaluate(&ctx(caller), &input).unwrap_err();
        assert!(err.to_string().contains("permissions:"), "got: {err}");
        assert!(err.to_string().contains("AgentScope"), "got: {err}");
    }

    #[test]
    fn send_to_agent_allows_in_scope_target() {
        let caller = AgentPermissions {
            scope: AgentScope {
                agent_ids: vec!["target-id".into()],
                ..AgentScope::default()
            },
            capabilities: vec![Capability::ControlAgent],
        };
        let input = SendToAgentInput {
            agent_id: "target-id".into(),
            content: "hello".into(),
            attachments: None,
        };
        let outcome = SendToAgentTool::evaluate(&ctx(caller), &input).unwrap();
        assert_eq!(outcome.target_agent_id, "target-id");
        assert_eq!(outcome.originating_user_id.as_deref(), Some("user-root"));
        assert!(
            !outcome.delivered,
            "no hook wired — runtime side-effect skipped"
        );
    }

    #[test]
    fn send_to_agent_universe_scope_allows_any_target() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let input = SendToAgentInput {
            agent_id: "anything".into(),
            content: "hi".into(),
            attachments: None,
        };
        assert!(SendToAgentTool::evaluate(&ctx(caller), &input).is_ok());
    }

    /// Cross-repo contract: a successful `send_to_agent` execution must
    /// stamp `reply_delivery=async_user_message` onto the `ToolResult`
    /// metadata. This is the signal the LLM-side prompt copy keys on to
    /// stop trying to read the target's reply out of the synchronous
    /// body and instead wait for the follow-up `user_message` that
    /// aura-os-server posts back into the caller's session when the
    /// target's turn finishes.
    #[tokio::test]
    async fn send_to_agent_marks_successful_result_with_async_reply_metadata() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let mut tctx = ctx(caller);
        tctx.agent_control_hook = Some(Arc::new(OkHook));

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({
                    "agent_id": "target-id",
                    "content": "hi",
                }),
            )
            .await
            .expect("execute");

        assert!(result.ok, "OkHook must produce a success result");
        assert_eq!(
            result.metadata.get("target_agent_id").map(String::as_str),
            Some("target-id"),
            "existing `target_agent_id` metadata must be preserved; got: {:?}",
            result.metadata
        );
        assert_eq!(
            result.metadata.get("reply_delivery").map(String::as_str),
            Some("async_user_message"),
            "successful send_to_agent must announce async reply delivery; got: {:?}",
            result.metadata
        );

        // The model-facing body must also spell out the async-reply
        // contract so the LLM waits for the reply to arrive on its own
        // instead of offering to poll the target's state.
        let outcome: SendToAgentOutcome = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            outcome.note.as_deref(),
            Some(ASYNC_REPLY_NOTE),
            "successful send_to_agent body must carry the async-reply note so the LLM \
             does not try to poll for the reply; got: {outcome:?}"
        );
    }

    /// The caller's resolved model id must be forwarded to the runtime
    /// side-effect layer as `model`, so the target agent's turn runs on
    /// a real model. Cross-agent recipients usually have no server-side
    /// configured model, so a missing model leaves the recipient's
    /// harness session empty and the turn fails with "model name must
    /// not be empty".
    #[tokio::test]
    async fn send_to_agent_forwards_caller_model_id_to_hook() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let mut tctx = ctx(caller);
        tctx.caller_model_id = Some("aura-claude-sonnet-4-6".into());
        let hook = CapturingHook::new();
        tctx.agent_control_hook = Some(hook.clone() as Arc<dyn crate::tool::AgentControlHook>);

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({ "agent_id": "target-id", "content": "hi" }),
            )
            .await
            .expect("execute");
        assert!(result.ok);

        assert_eq!(
            hook.captured_model().as_deref(),
            Some("aura-claude-sonnet-4-6"),
            "send_to_agent must forward the caller's model so the target turn has a model"
        );
    }

    /// Capturing hook that records the `parent_agent_id` value
    /// `send_to_agent` actually shipped to the runtime side-effect
    /// layer. Used by the cross-repo regression tests below to pin the
    /// "ship the upstream OS UUID, not the truncated harness hash"
    /// contract that lets the server's
    /// `spawn_cross_agent_reply_callback` resolve the originating
    /// agent's REST URL.
    struct CapturingHook {
        captured_parent: std::sync::Mutex<Option<String>>,
        captured_project: std::sync::Mutex<Option<String>>,
        captured_model: std::sync::Mutex<Option<String>>,
    }

    impl CapturingHook {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                captured_parent: std::sync::Mutex::new(None),
                captured_project: std::sync::Mutex::new(None),
                captured_model: std::sync::Mutex::new(None),
            })
        }
        fn captured(&self) -> Option<String> {
            self.captured_parent.lock().unwrap().clone()
        }
        fn captured_model(&self) -> Option<String> {
            self.captured_model.lock().unwrap().clone()
        }
        fn captured_project(&self) -> Option<String> {
            self.captured_project.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::tool::AgentControlHook for CapturingHook {
        async fn deliver_message(
            &self,
            _target_agent_id: &str,
            parent_agent_id: Option<&str>,
            _originating_user_id: Option<&str>,
            project_id: Option<&str>,
            _content: &str,
            _attachments: Option<serde_json::Value>,
            model: Option<&str>,
        ) -> Result<(), String> {
            *self.captured_parent.lock().unwrap() = parent_agent_id.map(str::to_string);
            *self.captured_project.lock().unwrap() = project_id.map(str::to_string);
            *self.captured_model.lock().unwrap() = model.map(str::to_string);
            Ok(())
        }
        async fn lifecycle(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn delegate_task(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
            _: Option<&serde_json::Value>,
            _model: Option<&str>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// Cross-repo regression: when the runtime has wired
    /// `caller_external_agent_id` (the upstream OS UUID), `send_to_agent`
    /// must ship THAT value as `parent_agent_id`, not the truncated
    /// harness blake3 hex from `caller_agent_id.to_string()`. The
    /// server-side `spawn_cross_agent_reply_callback` uses this id as
    /// the path segment when posting the recipient's reply back into
    /// the originator's session, and the route extractor parses it as
    /// `Uuid` — so a 16-char hex hash silently fails with 400 and the
    /// async-reply chain dies. See:
    ///   * `aura-os-server/src/handlers/agents/chat/cross_agent_reply.rs`
    ///   * `ToolContext::caller_external_agent_id` field doc.
    #[tokio::test]
    async fn send_to_agent_prefers_caller_external_agent_id_over_harness_hash() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let mut tctx = ctx(caller);
        tctx.caller_external_agent_id = Some("550e8400-e29b-41d4-a716-446655440000".into());
        let hook = CapturingHook::new();
        tctx.agent_control_hook = Some(hook.clone() as Arc<dyn crate::tool::AgentControlHook>);

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({
                    "agent_id": "target-id",
                    "content": "hi",
                }),
            )
            .await
            .expect("execute");
        assert!(result.ok);

        let captured = hook.captured();
        assert_eq!(
            captured.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000"),
            "send_to_agent must ship the upstream OS UUID as parent_agent_id, not the harness hash"
        );

        // The outcome body that the LLM observes must report the same
        // server-resolvable id so a downstream tool that reads the
        // outcome JSON also sees a valid UUID.
        let outcome: SendToAgentOutcome = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            outcome.parent_agent_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[tokio::test]
    async fn send_to_agent_forwards_the_callers_project() {
        let hook = CapturingHook::new();
        let mut tctx = ctx(AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        });
        tctx.caller_project_id = Some("project-123".into());
        tctx.agent_control_hook = Some(hook.clone());

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({ "agent_id": "target-id", "content": "hi" }),
            )
            .await
            .expect("execute");

        assert!(result.ok);
        assert_eq!(hook.captured_project().as_deref(), Some("project-123"));
    }

    /// Companion test: when the runtime has NOT wired
    /// `caller_external_agent_id` (legacy / in-process unit-test
    /// scaffolding), the tool falls back to
    /// `caller_agent_id.to_string()` so older code paths keep
    /// observing some non-empty parent id rather than `None`. This
    /// keeps backwards compatibility with existing integration tests
    /// that build `ToolContext` by hand without going through the
    /// runtime executor wiring.
    #[tokio::test]
    async fn send_to_agent_falls_back_to_caller_agent_id_when_external_missing() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let mut tctx = ctx(caller);
        assert!(
            tctx.caller_external_agent_id.is_none(),
            "fixture must leave external id unset to exercise the fallback path"
        );
        let hook = CapturingHook::new();
        tctx.agent_control_hook = Some(hook.clone() as Arc<dyn crate::tool::AgentControlHook>);

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({ "agent_id": "target-id", "content": "hi" }),
            )
            .await
            .expect("execute");
        assert!(result.ok);

        let captured = hook.captured().expect("captured parent must be Some");
        // `AgentId`'s Display truncates to 16 hex chars — the legacy
        // value we used to ship to the server.
        assert_eq!(captured.len(), 16, "fallback must be the truncated hash");
    }

    /// A blank/whitespace `caller_external_agent_id` must NOT clobber
    /// the fallback — defensive in case some upstream sets it to ""
    /// instead of clearing the field.
    #[tokio::test]
    async fn send_to_agent_treats_blank_external_id_as_unset() {
        let caller = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::ControlAgent],
        };
        let mut tctx = ctx(caller);
        tctx.caller_external_agent_id = Some("   ".into());
        let hook = CapturingHook::new();
        tctx.agent_control_hook = Some(hook.clone() as Arc<dyn crate::tool::AgentControlHook>);

        let result = SendToAgentTool
            .execute(
                &tctx,
                serde_json::json!({ "agent_id": "target-id", "content": "hi" }),
            )
            .await
            .expect("execute");
        assert!(result.ok);

        let captured = hook.captured().expect("captured parent must be Some");
        assert_eq!(
            captured.len(),
            16,
            "blank external id must be ignored and the fallback used"
        );
    }
}
