//! Phase 10 carve-out 5b — `PermissionRequest` kernel-side wiring.
//!
//! Verifies that when a [`PluginHookHost`] is attached to a
//! [`KernelConfig`], a registered `PermissionRequest` handler that
//! returns [`HookOutcome::Approve`] / [`HookOutcome::Deny`] short-
//! circuits the interactive [`ToolApprovalPrompter`] path.
//!
//! ## Scenarios
//!
//! 1. **Approve**: hook returns exit code 3 → kernel resolves the
//!    tri-state `ask` verdict to [`PolicyVerdict::Allow`]; the fake
//!    prompter is never invoked; the resulting [`ToolDecision`] is
//!    `Allowed`.
//! 2. **Deny**: hook returns exit code 4 (with a reason on stderr) →
//!    kernel resolves to [`PolicyVerdict::Deny { reason }`]; the
//!    fake prompter is never invoked; the resulting [`ToolDecision`]
//!    is `Denied` and surfaces the hook-supplied reason.
//! 3. **Fall-through**: hook returns exit code 0 (Continue) → kernel
//!    falls through to the interactive prompter; the fake records
//!    exactly one invocation.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use aura_kernel::{
    ExecutorRouter, Kernel, KernelConfig, PendingToolPrompt, ToolApprovalError,
    ToolApprovalPrompter, ToolApprovalRemember, ToolApprovalResponse,
};
use aura_plugin_hooks::{HookEngine, HookEvent, PluginHookHost, RegisteredHook};
use aura_reasoner::{MockProvider, ModelProvider};
use aura_store::{RocksStore, Store};
use std::time::Duration;
use tempfile::TempDir;

use async_trait::async_trait;
use aura_core::{AgentId, ToolProposal, Transaction, UserToolDefaults};

/// Tracking [`ToolApprovalPrompter`] that records invocation counts.
///
/// Returns [`ToolApprovalError::DeliveryFailed`] when `should_fail`
/// is `true` so a test can fail loudly if the prompter is reached
/// when the hook was supposed to short-circuit it.
#[derive(Debug)]
struct TrackingPrompter {
    calls: Mutex<u32>,
    /// `Some(response)` when the prompter should respond with the
    /// given value; `None` to fail with `DeliveryFailed`. Tests set
    /// this on the Approve / Deny scenarios to make any reach
    /// observable as a panic instead of a silent re-prompt.
    response: Mutex<Option<ToolApprovalResponse>>,
}

impl TrackingPrompter {
    fn new(response: Option<ToolApprovalResponse>) -> Self {
        Self {
            calls: Mutex::new(0),
            response: Mutex::new(response),
        }
    }

    fn call_count(&self) -> u32 {
        *self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl ToolApprovalPrompter for TrackingPrompter {
    async fn prompt(
        &self,
        _prompt: PendingToolPrompt,
    ) -> Result<ToolApprovalResponse, ToolApprovalError> {
        {
            let mut guard = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard += 1;
        }
        let response = *self
            .response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match response {
            Some(r) => Ok(r),
            None => Err(ToolApprovalError::DeliveryFailed),
        }
    }
}

#[cfg(unix)]
fn write_exit_script(dir: &Path, code: i32, stderr_msg: Option<&str>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(format!("hook-exit-{code}.sh"));
    let body = match stderr_msg {
        Some(msg) => format!("#!/bin/sh\necho '{msg}' 1>&2\nexit {code}\n"),
        None => format!("#!/bin/sh\nexit {code}\n"),
    };
    std::fs::write(&p, body).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[cfg(windows)]
fn write_exit_script(dir: &Path, code: i32, stderr_msg: Option<&str>) -> PathBuf {
    let p = dir.join(format!("hook-exit-{code}.cmd"));
    let body = match stderr_msg {
        Some(msg) => format!(
            "@echo off\r\necho {msg} 1>&2\r\nexit /b {code}\r\n",
            msg = msg,
            code = code
        ),
        None => format!("@echo off\r\nexit /b {code}\r\n", code = code),
    };
    std::fs::write(&p, body).unwrap();
    p
}

/// Build a [`PluginHookHost`] with a single registered hook for
/// `event` that exits with `code` (and writes `stderr_msg` to
/// stderr when supplied).
fn host_with_handler(
    plugin_root: &Path,
    event: HookEvent,
    code: i32,
    stderr_msg: Option<&str>,
) -> Arc<PluginHookHost> {
    let script = write_exit_script(plugin_root, code, stderr_msg);
    let engine = Arc::new(HookEngine::new().with_timeout(Duration::from_secs(2)));
    engine.register(RegisteredHook {
        plugin_id: "fixture".to_string(),
        event,
        command: script.to_string_lossy().into_owned(),
        args: Vec::new(),
        plugin_root: plugin_root.to_path_buf(),
        env: Default::default(),
    });
    Arc::new(PluginHookHost {
        engine,
        aura_home: plugin_root.to_path_buf(),
        session_id: "sess-test".to_string(),
        agent_id: "agent-test".to_string(),
        parent_agent_id: None,
    })
}

/// Common test scaffolding: build a kernel where every tool resolves
/// to `Ask`, attach the supplied prompter + plugin-hook host, and
/// invoke `read_file` as a tool proposal.
async fn run_ask_tool(
    prompter: Arc<TrackingPrompter>,
    plugin_hooks: Option<Arc<PluginHookHost>>,
) -> aura_kernel::ProcessResult {
    let db_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let agent_id = AgentId::generate();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));

    let policy =
        aura_kernel::PolicyConfig::default().with_user_default(UserToolDefaults::auto_review());
    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        policy,
        tool_approval_prompter: Some(prompter.clone() as Arc<dyn ToolApprovalPrompter>),
        originating_user_id: Some("u1".to_string()),
        plugin_hooks,
        ..KernelConfig::default()
    };

    let kernel = Kernel::new(store, provider, ExecutorRouter::new(), config, agent_id).unwrap();

    let proposal = ToolProposal::new(
        "tool-use-1",
        "read_file",
        serde_json::json!({ "path": "a.txt" }),
    );
    let tx = Transaction::tool_proposal(agent_id, &proposal).unwrap();
    kernel.process_direct(tx).await.unwrap()
}

#[tokio::test]
async fn permission_request_hook_approve_short_circuits_prompter() {
    let tmp = TempDir::new().unwrap();
    let host = host_with_handler(tmp.path(), HookEvent::PermissionRequest, 3, None);
    // The prompter is rigged to fail loudly if reached.
    let prompter = Arc::new(TrackingPrompter::new(None));

    let result = run_ask_tool(prompter.clone(), Some(host)).await;

    assert_eq!(
        prompter.call_count(),
        0,
        "the interactive prompter must NEVER be reached when PermissionRequest hook approves"
    );
    let decision = result
        .tool_decision
        .as_ref()
        .expect("tool decision should be set on the tool-proposal path");
    assert!(
        matches!(decision, aura_kernel::ToolDecision::Allowed),
        "hook Approve must produce ToolDecision::Allowed, got {decision:?}"
    );
}

#[tokio::test]
async fn permission_request_hook_deny_short_circuits_prompter() {
    let tmp = TempDir::new().unwrap();
    let host = host_with_handler(
        tmp.path(),
        HookEvent::PermissionRequest,
        4,
        Some("denied by policy plugin"),
    );
    let prompter = Arc::new(TrackingPrompter::new(None));

    let result = run_ask_tool(prompter.clone(), Some(host)).await;

    assert_eq!(
        prompter.call_count(),
        0,
        "the interactive prompter must NEVER be reached when PermissionRequest hook denies"
    );
    let decision = result
        .tool_decision
        .as_ref()
        .expect("tool decision should be set on the tool-proposal path");
    match decision {
        aura_kernel::ToolDecision::Denied { reason } => {
            assert!(
                reason.contains("denied by policy plugin"),
                "deny reason must surface the hook's stderr, got: {reason}"
            );
        }
        other => panic!("hook Deny must produce ToolDecision::Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn permission_request_hook_continue_falls_through_to_prompter() {
    let tmp = TempDir::new().unwrap();
    let host = host_with_handler(tmp.path(), HookEvent::PermissionRequest, 0, None);
    // The prompter responds with Deny so the test deterministically
    // sees the interactive prompt was consulted exactly once.
    let prompter = Arc::new(TrackingPrompter::new(Some(ToolApprovalResponse {
        decision: aura_core::ToolState::Deny,
        remember: ToolApprovalRemember::Once,
    })));

    let result = run_ask_tool(prompter.clone(), Some(host)).await;

    assert_eq!(
        prompter.call_count(),
        1,
        "Continue outcome must fall through to the interactive prompter exactly once"
    );
    // The interactive prompter denied, so the verdict is Denied.
    assert!(
        matches!(
            result.tool_decision,
            Some(aura_kernel::ToolDecision::Denied { .. })
        ),
        "interactive deny must surface as ToolDecision::Denied, got {:?}",
        result.tool_decision
    );
}
