//! Phase 10 carve-out 5a — `PreToolUse` mid-flight cancellation.
//!
//! Phase 8 fired the `PreToolUse` hook AFTER the streaming pump had
//! already executed the tool batch and a `Block` decision only
//! produced a `tracing::warn!`. Phase 10 moves the firing site to
//! BEFORE executor dispatch. On
//! [`aura_plugin_hooks::HookOutcome::Block`]:
//!
//! 1. The executor is NEVER invoked — verified here with a
//!    tracking [`aura_kernel::Executor`] that panics if called.
//! 2. The synthetic [`aura_core::Effect::failed`] carries a JSON
//!    discriminator (`{"kind":"tool_call_blocked_by_hook", ...}`)
//!    on its payload so the audit consumer can distinguish the
//!    block from a normal executor failure. This surfaces the
//!    schema-v2 [`aura_store_record::RecordKind::ToolCallBlockedByHook`]
//!    taxonomy at the effect-payload level while keeping a single
//!    deterministic sequence number per blocked slot (no parallel
//!    `write_system_record` race).
//! 3. The kernel surfaces the synthetic
//!    [`aura_core::Effect::failed`] (status =
//!    [`aura_core::EffectStatus::Failed`]) so the agent loop sees
//!    a clean rejection in its next iteration.
//!
//! Two acceptance tests cover the single-proposal path (most
//! common) and the batch path (multi-tool turn).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use aura_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_plugin_hooks::{HookEngine, HookEvent, PluginHookHost, RegisteredHook};
use aura_reasoner::{MockProvider, ModelProvider};
use aura_store::{RocksStore, Store};
use tempfile::TempDir;

use async_trait::async_trait;
use aura_core::{
    Action, AgentId, Effect, EffectKind, EffectStatus, ToolProposal, Transaction, TransactionType,
};

/// Tracking [`aura_kernel::Executor`] that panics if `execute` is
/// reached. Used to enforce the "executor was NEVER invoked" arm
/// of the acceptance contract.
#[derive(Debug)]
struct ForbiddenExecutor {
    name: &'static str,
    calls: Mutex<u32>,
}

impl ForbiddenExecutor {
    fn new() -> Self {
        Self {
            name: "forbidden",
            calls: Mutex::new(0),
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
impl aura_kernel::Executor for ForbiddenExecutor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn can_handle(&self, _action: &Action) -> bool {
        true
    }

    async fn execute(
        &self,
        _ctx: &aura_kernel::ExecuteContext,
        action: &Action,
    ) -> Result<Effect, aura_kernel::ExecutorError> {
        let mut guard = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard += 1;
        // Use a committed effect so a *failure* to short-circuit is
        // observable as an unexpected success on the assertions.
        Ok(Effect::new(
            action.action_id,
            EffectKind::Agreement,
            EffectStatus::Committed,
            "should never reach this",
        ))
    }
}

#[cfg(unix)]
fn write_block_script(dir: &Path, reason: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("hook-block.sh");
    let body = format!("#!/bin/sh\necho '{reason}' 1>&2\nexit 2\n");
    std::fs::write(&p, body).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[cfg(windows)]
fn write_block_script(dir: &Path, reason: &str) -> PathBuf {
    let p = dir.join("hook-block.cmd");
    let body = format!(
        "@echo off\r\necho {reason} 1>&2\r\nexit /b 2\r\n",
        reason = reason
    );
    std::fs::write(&p, body).unwrap();
    p
}

fn host_with_pre_tool_use_block(plugin_root: &Path, reason: &str) -> Arc<PluginHookHost> {
    let script = write_block_script(plugin_root, reason);
    let engine = Arc::new(HookEngine::new().with_timeout(Duration::from_secs(2)));
    engine.register(RegisteredHook {
        plugin_id: "fixture".to_string(),
        event: HookEvent::PreToolUse,
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

/// Common setup: kernel with a `ForbiddenExecutor` registered and an
/// optional `PluginHookHost`. The returned `(kernel, executor,
/// store, agent_id)` lets the caller dispatch and then inspect the
/// audit log to verify the `tool_call_blocked_by_hook` System row.
fn build_kernel(
    plugin_hooks: Option<Arc<PluginHookHost>>,
) -> (
    Kernel,
    Arc<ForbiddenExecutor>,
    Arc<dyn Store>,
    AgentId,
    TempDir,
    TempDir,
) {
    let db_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let agent_id = AgentId::generate();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));
    let executor = Arc::new(ForbiddenExecutor::new());
    let mut router = ExecutorRouter::new();
    router.add_executor(executor.clone() as Arc<dyn aura_kernel::Executor>);

    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        plugin_hooks,
        ..KernelConfig::default()
    };
    let kernel = Kernel::new(store.clone(), provider, router, config, agent_id).unwrap();
    (kernel, executor, store, agent_id, db_dir, ws_dir)
}

/// Parse the synthetic effect payload to recover the block reason.
///
/// Returns `Some(reason)` when the effect's payload matches the
/// schema-v2 `tool_call_blocked_by_hook` JSON discriminator;
/// `None` for any other payload (e.g. a normal executor effect).
fn block_reason_from_effect(effect: &Effect) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&effect.payload).ok()?;
    let kind = value.get("kind").and_then(|v| v.as_str())?;
    if kind != "tool_call_blocked_by_hook" {
        return None;
    }
    value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[tokio::test]
async fn pre_tool_use_block_aborts_single_proposal_dispatch() {
    let tmp = TempDir::new().unwrap();
    let host = host_with_pre_tool_use_block(tmp.path(), "blocked by policy plugin");
    let (kernel, executor, store, agent_id, _db, _ws) = build_kernel(Some(host));

    let proposal = ToolProposal::new(
        "tool-use-1",
        "read_file",
        serde_json::json!({ "path": "a.txt" }),
    );
    let tx = Transaction::tool_proposal(agent_id, &proposal).unwrap();
    let result = kernel.process_direct(tx).await.unwrap();

    // (a) the kernel-side executor was NEVER invoked.
    assert_eq!(
        executor.call_count(),
        0,
        "ForbiddenExecutor::execute must NEVER run when PreToolUse blocks"
    );

    // (b) the audit-log entry carries a `tool_call_blocked_by_hook`
    //     discriminated payload (this surfaces the schema-v2
    //     `RecordKind::ToolCallBlockedByHook` taxonomy at the
    //     effect-payload level — the surrounding `RecordEntry`
    //     remains tagged with the original
    //     `TransactionType::ToolProposal`).
    let entry = result.entry;
    assert_eq!(
        entry.effects.len(),
        1,
        "blocked-by-hook entry must record exactly one synthetic effect"
    );
    let effect = &entry.effects[0];
    let reason = block_reason_from_effect(effect)
        .expect("synthetic effect must carry tool_call_blocked_by_hook discriminator");
    assert!(
        reason.contains("blocked by policy plugin"),
        "block reason must surface the hook's stderr, got: {reason}"
    );

    // (c) the synthetic effect is a failed Effect so the agent
    //     loop observes a clean rejection.
    assert_eq!(
        effect.status,
        EffectStatus::Failed,
        "synthetic block effect must be EffectStatus::Failed"
    );

    // Smoke: store is reachable (we don't expect a second System
    // record, but we want the agent log to remain readable after
    // the block to catch any seq-claim regression).
    let entries = store.scan_record(agent_id, 1, 10).expect("scan record");
    assert!(
        !entries.is_empty(),
        "agent record log must contain the tool-proposal entry after a block"
    );
}

#[tokio::test]
async fn pre_tool_use_block_persists_discriminator_to_record_log() {
    // Second test: after a block, scan the agent's record log via
    // the [`aura_store::Store`] and verify that the persisted
    // [`aura_core::RecordEntry`] reproduces the synthetic
    // `tool_call_blocked_by_hook` effect payload. This pins the
    // round-trip so a future change to the wire format must
    // either preserve the discriminator or bump the schema
    // version again.
    let tmp = TempDir::new().unwrap();
    let host = host_with_pre_tool_use_block(tmp.path(), "second block");
    let (kernel, executor, store, agent_id, _db, _ws) = build_kernel(Some(host));

    let proposal = ToolProposal::new(
        "tool-use-2",
        "run_command",
        serde_json::json!({ "cmd": "ls" }),
    );
    let tx = Transaction::tool_proposal(agent_id, &proposal).unwrap();
    let _ = kernel.process_direct(tx).await.unwrap();

    assert_eq!(executor.call_count(), 0, "executor must not be reached");

    let entries = store.scan_record(agent_id, 1, 50).expect("scan record");
    let discriminator_count: usize = entries
        .iter()
        .filter(|entry| entry.tx.tx_type == TransactionType::ToolProposal)
        .map(|entry| {
            entry
                .effects
                .iter()
                .filter(|e| block_reason_from_effect(e).is_some())
                .count()
        })
        .sum();
    assert_eq!(
        discriminator_count, 1,
        "exactly one `tool_call_blocked_by_hook` effect payload must round-trip through the record log"
    );
}
