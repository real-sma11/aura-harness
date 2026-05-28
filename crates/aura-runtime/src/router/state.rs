//! Shared router state and its [`RouterStateConfig`] input bundle.
//!
//! Split out of `mod.rs` so the dispatch root only owns the slim
//! `create_router` mounting logic. Field visibility stays `pub(crate)`
//! because the route handlers live in sibling modules
//! (`automaton.rs`, `tx.rs`, `files.rs`, `memory/`, `ws.rs`, …) and
//! reach into the state directly. External callers must always go
//! through [`RouterState::new`].

use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use tokio::sync::Semaphore;

use aura_reasoner::ModelProvider;
use aura_store::Store;
use aura_tools::automaton_tools::AutomatonController;
use aura_tools::domain_tools::DomainApi;
use aura_tools::{ToolCatalog, ToolConfig};

use crate::automaton_bridge::AutomatonBridge;
use crate::config::NodeConfig;
use crate::scheduler::Scheduler;

use super::ws;

/// Shared state for the router.
///
/// Fields are `pub(crate)` — external callers (including the `test_support`
/// feature and harness binaries) must go through [`RouterState::new`]. This
/// keeps the wire-up in one place instead of scattering struct literals
/// across test fixtures. (Wave 3 — T2.3.)
pub struct RouterState {
    // TODO(phase2-followup): Invariant §10 wants this bound to
    // `Arc<dyn ReadStore>`. The router itself only needs `enqueue_tx`
    // / `get_head_seq` / `has_pending_tx` (all on `ReadStore`), but it
    // also hands the store to `WsContext`, which in turn hands it to
    // `Kernel::new` — and `Kernel::new` takes `Arc<dyn Store>`.
    // Resolving this requires either (a) teaching `Kernel::new` to
    // accept a `(ReadStore, WriteHook)` pair or (b) splitting this
    // field into a `ReadStore` for the HTTP surface and a separate
    // `Store` scoped to the session/kernel construction path. Punted
    // to a follow-up phase.
    pub(crate) store: Arc<dyn Store>,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) config: NodeConfig,
    /// Model provider for WebSocket sessions (type-erased).
    pub(crate) provider: Arc<dyn ModelProvider + Send + Sync>,
    /// Tool configuration for WebSocket sessions.
    pub(crate) tool_config: ToolConfig,
    /// Canonical tool catalog (shared across sessions).
    pub(crate) catalog: Arc<ToolCatalog>,
    /// Domain API for specs/tasks/project/orbit/network (None if no internal token).
    pub(crate) domain_api: Option<Arc<dyn DomainApi>>,
    /// Automaton controller for dev-loop lifecycle (None when domain API unavailable).
    pub(crate) automaton_controller: Option<Arc<dyn AutomatonController>>,
    /// Concrete bridge for event subscription (same object as automaton_controller).
    pub(crate) automaton_bridge: Option<Arc<AutomatonBridge>>,
    /// tx_id (hex) -> error message for scheduling failures after 202 acceptance.
    pub(crate) failed_txs: Arc<DashMap<String, String>>,
    /// Optional memory manager for CRUD API and session injection.
    pub(crate) memory_manager: Option<Arc<aura_memory::MemoryManager>>,
    /// Optional skill manager for skill CRUD API and prompt injection.
    pub(crate) skill_manager: Option<Arc<RwLock<aura_skills::SkillManager>>>,
    /// Router URL for generation proxying (from `AURA_ROUTER_URL`).
    pub(crate) router_url: Option<String>,
    /// Bounded pool of WebSocket connection slots.
    ///
    /// Every upgrade handler (`/ws/terminal`, `/stream/:run_id`) must call
    /// [`super::ws::try_acquire_ws_slot`] and attach the returned permit to
    /// the spawned socket task. When the semaphore is empty, the handler
    /// short-circuits with `503 Service Unavailable` instead of tying
    /// up another tokio task.
    pub(crate) ws_slots: Arc<Semaphore>,
    /// `run_id` (UUID string) → fully-prepared chat [`crate::session::Session`]
    /// awaiting a WS attach.
    ///
    /// Phase A: `POST /v1/run` applies the [`aura_protocol::RuntimeRequest`]
    /// synchronously, stashes the prepared session here, and returns
    /// `{run_id, event_stream_url}`. The follow-up `WS /stream/:run_id`
    /// removes the entry and hands the session to
    /// [`crate::session::handle_chat_ws_connection`]. The map carries a
    /// `Mutex<Option<Session>>` so a late WS reconnect attempt against an
    /// already-attached run finds `None` and falls through to a 404, while
    /// the DashMap key still serves as the disambiguation seam against the
    /// automaton-run path.
    pub(crate) pending_chat_runs:
        Arc<DashMap<String, std::sync::Mutex<Option<crate::session::Session>>>>,
}

/// Input bundle for [`RouterState::new`].
///
/// Grouped into a single parameter struct so we don't have to thread 13
/// positional arguments through every test and binary. Optional fields
/// mirror the ones that default to `None` on the router state.
pub struct RouterStateConfig {
    /// Store handle for HTTP/WS endpoints. See the TODO on
    /// [`RouterState::store`] — the type is conceptually
    /// `Arc<dyn ReadStore>` (Invariant §10) but is still `Arc<dyn Store>`
    /// while the kernel constructor expects the combined trait.
    pub store: Arc<dyn Store>,
    pub scheduler: Arc<Scheduler>,
    pub config: NodeConfig,
    pub provider: Arc<dyn ModelProvider + Send + Sync>,
    pub tool_config: ToolConfig,
    pub catalog: Arc<ToolCatalog>,
    pub domain_api: Option<Arc<dyn DomainApi>>,
    pub automaton_controller: Option<Arc<dyn AutomatonController>>,
    pub automaton_bridge: Option<Arc<AutomatonBridge>>,
    pub memory_manager: Option<Arc<aura_memory::MemoryManager>>,
    pub skill_manager: Option<Arc<RwLock<aura_skills::SkillManager>>>,
    pub router_url: Option<String>,
}

impl RouterState {
    /// Build a router state from the given configuration.
    ///
    /// `failed_txs` is always initialized fresh — there is no legitimate
    /// reason to share that map across `RouterState` instances.
    #[must_use]
    pub fn new(cfg: RouterStateConfig) -> Self {
        Self {
            store: cfg.store,
            scheduler: cfg.scheduler,
            config: cfg.config,
            provider: cfg.provider,
            tool_config: cfg.tool_config,
            catalog: cfg.catalog,
            domain_api: cfg.domain_api,
            automaton_controller: cfg.automaton_controller,
            automaton_bridge: cfg.automaton_bridge,
            failed_txs: Arc::new(DashMap::new()),
            memory_manager: cfg.memory_manager,
            skill_manager: cfg.skill_manager,
            router_url: cfg.router_url,
            ws_slots: Arc::new(Semaphore::new(ws::MAX_WS_CONNS_PER_NODE)),
            pending_chat_runs: Arc::new(DashMap::new()),
        }
    }
}

impl Clone for RouterState {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            scheduler: self.scheduler.clone(),
            config: self.config.clone(),
            provider: self.provider.clone(),
            tool_config: self.tool_config.clone(),
            catalog: self.catalog.clone(),
            domain_api: self.domain_api.clone(),
            automaton_controller: self.automaton_controller.clone(),
            automaton_bridge: self.automaton_bridge.clone(),
            failed_txs: self.failed_txs.clone(),
            memory_manager: self.memory_manager.clone(),
            skill_manager: self.skill_manager.clone(),
            router_url: self.router_url.clone(),
            ws_slots: self.ws_slots.clone(),
            pending_chat_runs: self.pending_chat_runs.clone(),
        }
    }
}
