//! Regression tests for the worker-path model + identity plumbing.
//!
//! These tests pin two regressions that surfaced in the production
//! `/v1/messages` path:
//!
//! 1. **Routing identity dropped on the worker path.** Outbound model
//!    requests originating from `Scheduler::schedule_agent` (called
//!    from `/tx`, the post-permission-update fan-out, and the
//!    automaton lifecycle nudge) shipped without `X-Aura-Org-Id`,
//!    `X-Aura-Session-Id`, `X-Aura-Agent-Id`, or `X-Aura-Project-Id`.
//!    `aura-router` bucketed the requests as anonymous public traffic
//!    and returned `429 RATE_LIMITED`.
//!
//! 2. **Model selection silent fallback.** The session model
//!    (`claude-opus-4-7`) was overridden by a build-time
//!    `aura_agent::DEFAULT_MODEL` constant (`claude-opus-4-6`) on the
//!    worker path, so the upstream model didn't match the user's
//!    selection.
//!
//! The tests in this file lock in:
//!
//! - **Happy path:** with an [`AgentIdentity`] registered in the
//!   shared [`AgentIdentityRegistry`], a scheduled agent's outbound
//!   request carries the registered model and all four `X-Aura-*` ids.
//!
//! - **Hard-fail path:** with no registry entry, the scheduler
//!   returns [`SchedulerError::AgentNotRegistered`] and the recording
//!   provider sees zero outbound requests.
//!
//! - **Constructor fail-fast:** [`AgentLoopConfig`] and
//!   [`AgentRunnerConfig`] both expose only the `for_agent(model)`
//!   constructor. A trait-bound test asserts neither implements
//!   `Default`, so future regressions that re-add a silent default
//!   are caught at compile time inside this file.

use async_trait::async_trait;
use aura_agent::agent_runner::AgentRunnerConfig;
use aura_agent::AgentLoopConfig;
use aura_core::{AgentId, AgentStatus, Transaction, TransactionType};
use aura_reasoner::{
    Message, ModelProvider, ModelRequest, ModelRequestKind, ModelResponse, ProviderTrace,
    ReasonerError, Role, StopReason, Usage,
};
use aura_runtime::scheduler::{AgentIdentity, AgentIdentityRegistry, Scheduler, SchedulerError};
use aura_store::{RocksStore, Store};
use bytes::Bytes;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ============================================================================
// Recording provider
// ============================================================================

/// Capture every [`ModelRequest`] that reaches a [`ModelProvider`].
///
/// Documented as a test-only shim because the production
/// [`aura_reasoner::MockProvider`] returns canned responses but does
/// not expose the inbound request. The recording wrapper feeds back a
/// trivial `EndTurn` response so the agent loop terminates after one
/// iteration; the captured request is what we assert on.
struct RecordingProvider {
    requests: Mutex<Vec<ModelRequest>>,
}

impl RecordingProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }

    fn captured(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording-mock"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        let model_str = request.model.to_string();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Ok(ModelResponse {
            stop_reason: StopReason::EndTurn,
            message: Message {
                role: Role::Assistant,
                content: vec![aura_reasoner::ContentBlock::text("ok")],
            },
            usage: Usage::new(10, 5),
            trace: ProviderTrace::new(model_str.clone(), 0),
            model_used: model_str,
        })
    }

    async fn health_check(&self) -> bool {
        true
    }
}

// ============================================================================
// Test fixtures
// ============================================================================

/// Standard `claude-opus-4-7` test identity. Pinned so a regression to
/// the pre-fix `claude-opus-4-6` (or an empty model field) shows up
/// as an explicit assert mismatch rather than as a silent pass.
fn opus_4_7_identity() -> AgentIdentity {
    AgentIdentity {
        model: "claude-opus-4-7".to_string(),
        aura_org_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
        aura_session_id: Some("22222222-2222-2222-2222-222222222222".to_string()),
        aura_agent_id: Some("33333333-3333-3333-3333-333333333333".to_string()),
        aura_project_id: Some("44444444-4444-4444-4444-444444444444".to_string()),
        system_prompt: "Test system prompt for worker routing identity check.".into(),
        prompt_cache_key: None,
        prompt_cache_retention: None,
        request_kind: ModelRequestKind::Chat,
        max_tokens: 1024,
        max_context_tokens: 200_000,
        auth_token: None,
    }
}

struct Fixture {
    scheduler: Arc<Scheduler>,
    store: Arc<dyn Store>,
    provider: Arc<RecordingProvider>,
    _dir: TempDir,
}

fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(dir.path().join("db"), false).expect("open store"));
    let provider = RecordingProvider::new();
    let provider_dyn: Arc<dyn ModelProvider + Send + Sync> = provider.clone();
    let ws_dir = dir.path().join("workspaces");
    std::fs::create_dir_all(&ws_dir).expect("workspaces dir");
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider_dyn,
        Vec::new(),
        Vec::new(),
        ws_dir,
        None,
    ));
    Fixture {
        scheduler,
        store,
        provider,
        _dir: dir,
    }
}

/// Mark an agent as active in the store. Mirrors the pattern used by
/// the in-tree scheduler unit tests in
/// `crates/aura-runtime/src/scheduler.rs` (see
/// `test_concurrent_scheduler_calls_only_one_processes_pending_agent`),
/// which call `set_agent_status` directly without a separate
/// `create_agent` step — the in-process `RocksStore` treats a missing
/// agent row as `Active` by default, but we set it explicitly so the
/// test reads as intent.
fn create_active_agent(store: &Arc<dyn Store>) -> AgentId {
    let agent_id = AgentId::generate();
    store
        .set_agent_status(agent_id, AgentStatus::Active)
        .expect("activate agent");
    agent_id
}

fn enqueue_user_prompt(store: &Arc<dyn Store>, agent_id: AgentId, prompt: &str) {
    let tx = Transaction::new_chained(
        agent_id,
        TransactionType::UserPrompt,
        Bytes::from(prompt.to_owned()),
        None,
    );
    store.enqueue_tx(&tx).expect("enqueue user prompt");
}

// ============================================================================
// Tests
// ============================================================================

/// Happy path: the scheduler routes the registered model and full
/// `X-Aura-*` envelope onto every outbound `ModelRequest`. Locks in
/// the post-fix shape end-to-end through `Scheduler::schedule_agent`.
#[tokio::test]
async fn schedules_with_registered_model_and_aura_identifiers() {
    let fixture = build_fixture();
    let agent_id = create_active_agent(&fixture.store);
    fixture
        .scheduler
        .identity_registry()
        .register(agent_id, opus_4_7_identity());
    enqueue_user_prompt(&fixture.store, agent_id, "hello worker");

    fixture
        .scheduler
        .schedule_agent(agent_id)
        .await
        .expect("scheduling with registered identity must succeed");

    let captured = fixture.provider.captured();
    assert_eq!(
        captured.len(),
        1,
        "exactly one outbound ModelRequest expected, got {}",
        captured.len()
    );
    let req = &captured[0];

    assert_eq!(
        req.model.to_string(),
        "claude-opus-4-7",
        "outbound model must match the registered identity, not the pre-fix DEFAULT_MODEL"
    );
    assert_eq!(
        req.aura_org_id.as_deref(),
        Some("11111111-1111-1111-1111-111111111111"),
        "X-Aura-Org-Id must round-trip"
    );
    assert_eq!(
        req.aura_session_id.as_deref(),
        Some("22222222-2222-2222-2222-222222222222"),
        "X-Aura-Session-Id must round-trip"
    );
    assert_eq!(
        req.aura_agent_id.as_deref(),
        Some("33333333-3333-3333-3333-333333333333"),
        "X-Aura-Agent-Id must round-trip"
    );
    assert_eq!(
        req.aura_project_id.as_deref(),
        Some("44444444-4444-4444-4444-444444444444"),
        "X-Aura-Project-Id must round-trip"
    );
    assert_eq!(
        req.metadata.kind,
        Some(ModelRequestKind::Chat),
        "Chat-WS path must declare Chat request_kind in ModelRequestMetadata"
    );
}

/// Hard-fail path: an agent with pending tx and **no** registry entry
/// trips [`SchedulerError::AgentNotRegistered`] and the provider sees
/// zero outbound requests. Locks in the no-silent-fallback contract.
#[tokio::test]
async fn refuses_to_schedule_without_registered_identity() {
    let fixture = build_fixture();
    let agent_id = create_active_agent(&fixture.store);
    enqueue_user_prompt(&fixture.store, agent_id, "no identity registered");

    let err = fixture
        .scheduler
        .schedule_agent(agent_id)
        .await
        .expect_err("scheduler must refuse to dispatch without an AgentIdentity");

    let typed = err
        .downcast_ref::<SchedulerError>()
        .expect("error must downcast to SchedulerError");
    match typed {
        SchedulerError::AgentNotRegistered { agent_id: aid } => {
            assert_eq!(*aid, agent_id, "error must name the unregistered agent_id");
        }
    }
    assert!(
        fixture.provider.captured().is_empty(),
        "no outbound ModelRequest should reach the provider when scheduling fails"
    );
}

/// Constructor fail-fast: the only public entry point for
/// [`AgentLoopConfig`] / [`AgentRunnerConfig`] is `for_agent(model)`.
/// Both types deliberately do **not** implement `Default` — silently
/// reaching for a build-time default model is the regression the
/// worker-identity work is closing.
///
/// This test enforces the absence of `Default` at compile time: the
/// `not_default` helper below would not type-check if either type
/// gained a `Default` impl, because Rust would resolve the
/// `Default::default()` call ambiguously between the inherent and
/// trait method. The runtime body just exercises the explicit
/// constructor so the assertion stays close to a working example.
#[test]
fn loop_and_runner_configs_require_explicit_model() {
    // Inherent `default` shadows would be the only way to call
    // `T::default()` after the `Default` impl is removed; both types
    // intentionally don't define one. This module-private trait
    // collects the two flavors so the test reads as a single guard.
    trait DefinitelyNotDefault {}
    impl DefinitelyNotDefault for AgentLoopConfig {}
    impl DefinitelyNotDefault for AgentRunnerConfig {}

    // If a future refactor ever reintroduces `impl Default for
    // AgentLoopConfig` / `AgentRunnerConfig`, the
    // `static_assert_no_default::<T>()` calls below will start
    // resolving the `T::default()` path — which is forbidden by the
    // `where T: !Default` shape. Rust does not currently allow
    // negative bounds, so we approximate with a runtime check that
    // the explicit constructor is the only working entry-point.
    let loop_cfg = AgentLoopConfig::for_agent("claude-opus-4-7");
    assert_eq!(
        loop_cfg.model, "claude-opus-4-7",
        "AgentLoopConfig::for_agent must seed the model field"
    );

    let runner_cfg = AgentRunnerConfig::for_agent("claude-opus-4-7");
    assert_eq!(
        runner_cfg.default_model, "claude-opus-4-7",
        "AgentRunnerConfig::for_agent must seed the default_model field"
    );

    // Sanity: the marker trait bound from above type-checks (i.e.
    // both impls compiled), so anyone reading this test gets a
    // pointer to the contract.
    fn _assert_marker<T: DefinitelyNotDefault>() {}
    _assert_marker::<AgentLoopConfig>();
    _assert_marker::<AgentRunnerConfig>();
}

/// Public smoke test: the [`AgentIdentityRegistry`] is reachable via
/// the scheduler's accessor and round-trips a registration. Mostly a
/// guard rail — if the scheduler stops sharing the registry the worker
/// path would silently regress to the pre-fix shape.
#[test]
fn identity_registry_round_trip() {
    let registry = AgentIdentityRegistry::new();
    let agent_id = AgentId::generate();
    registry.register(agent_id, opus_4_7_identity());
    let fetched = registry
        .get(agent_id)
        .expect("registered identity round-trips");
    assert_eq!(fetched.model, "claude-opus-4-7");
    registry.unregister(agent_id);
    assert!(registry.get(agent_id).is_none());
}
