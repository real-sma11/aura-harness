//! Shared test scaffolding for the `aura-fleet-spawn` integration
//! tests.
//!
//! Provides a fake [`ChildRunner`] (delay-only or echo) and a
//! convenience builder for parent contexts at any [`AgentMode`] so
//! each integration test focuses on the per-scenario assertion.

#![allow(dead_code)] // each test only consumes a subset of the helpers

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentSpec};
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode};
use aura_core_permissions::{Capability, Permissions};
use aura_core_types::{AgentId, SubagentExit, SubagentResult};
use aura_fleet_spawn::{ChildRunContext, ChildRunError, ChildRunner, OrphanStore};
use aura_store_db::{RocksStore, Store};
use parking_lot::Mutex;

/// Construct a parent context at the requested [`AgentMode`] with
/// `SpawnAgent`-capable permissions (so derivation rejects ONLY on
/// the mode gate, not on permission narrowing). The optional
/// `depth` argument controls the parent's depth — used by the
/// depth-exceeded test.
pub fn parent_at(mode: AgentMode, depth: u32) -> ParentContext {
    let agent_id = AgentId::generate();
    let profile = ModeProfile {
        agent: mode,
        kernel: KernelMode::Audited,
        sandbox: SandboxMode::Standard,
        replay: ReplayMode::Live,
    };
    ParentContext {
        agent_id,
        depth,
        mode,
        mode_profile: profile,
        permissions: Permissions {
            scope: aura_core_permissions::AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent, Capability::ReadAllProjects],
        },
        model_id: "claude-opus-4-7".to_string(),
        lineage: SubagentLineage::from_root(agent_id),
    }
}

/// Open a fresh RocksDB-backed store in a `tempfile` directory.
/// Returns the store handle plus the keep-alive tempdir.
pub fn open_test_store() -> (Arc<dyn Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = RocksStore::open(dir.path().join("db"), false).expect("open rocks store");
    (Arc::new(store), dir)
}

/// Construct an [`OrphanStore`] rooted in a tempdir. Returns the
/// store plus the tempdir keep-alive (drop the tempdir to remove
/// every orphan file written through the store).
pub fn open_test_orphan_store() -> (Arc<OrphanStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = OrphanStore::new(dir.path().to_path_buf());
    (Arc::new(store), dir)
}

/// A [`ChildRunner`] that records every invocation and either
/// returns a fixed [`SubagentResult`] or sleeps for a fixed
/// duration before doing so.
#[derive(Default)]
pub struct FakeChildRunner {
    /// Optional per-invocation wall-clock delay used by the
    /// independent-parents parallelism test.
    pub delay: Option<Duration>,
    /// Captured (spec, prompt) pairs, in invocation order.
    pub calls: Mutex<Vec<(SubagentSpec, String)>>,
}

impl FakeChildRunner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_delay(delay: Duration) -> Self {
        Self {
            delay: Some(delay),
            calls: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn invocation_count(&self) -> usize {
        self.calls.lock().len()
    }
}

#[async_trait]
impl ChildRunner for FakeChildRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        let preassigned = ctx.preassigned_agent_id;
        self.calls.lock().push((ctx.spec.clone(), ctx.prompt));
        if let Some(d) = self.delay {
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = ctx.cancellation.cancelled() => {
                    return Ok(SubagentResult {
                        child_agent_id: Some(preassigned),
                        final_message: String::new(),
                        total_input_tokens: 0,
                        total_output_tokens: 0,
                        files_changed: Vec::new(),
                        exit: SubagentExit::Cancelled,
                    });
                }
            }
        }
        Ok(SubagentResult {
            child_agent_id: Some(preassigned),
            final_message: "child done".to_string(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit: SubagentExit::Completed,
        })
    }
}
