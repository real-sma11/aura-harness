//! WebSocket session state and lifecycle.
//!
//! Each WebSocket connection maps to a `Session` that maintains conversation
//! state, tool configuration, and token accounting across turns.
//!
//! ## Layout
//!
//! - [`state`] — the `Session` struct, its `new` /
//!   `apply_chat_runtime_request` lifecycle, the wire→core permission +
//!   intent-classifier conversions. Split out in Wave 6 / T3 so this file can
//!   stay a thin facade.
//! - [`generation`] — SSE proxy for generation (images / 3D).
//! - [`helpers`] — turn execution helpers shared by the WebSocket path.
//! - [`partial_json`] — partial-JSON repair used during streaming.
//! - [`ws_handler`] — top-level WebSocket handler and turn orchestration.
//! - [`tests`] — unit tests extracted alongside the state split.

mod chat;
pub(crate) mod cross_agent_hook;
mod generation;
mod helpers;
mod partial_json;
mod state;
#[cfg(test)]
mod tests;
mod tool_approval;

pub(crate) use chat::handle_chat_ws_connection;
pub(crate) use helpers::{prepare_chat_session, ChatRequestError};
pub(crate) use state::agent_permissions_from_wire;
pub use state::Session;
pub(crate) use tool_approval::ToolApprovalBroker;

use aura_engine::scheduler::Scheduler;
use aura_reasoner::ModelProvider;
use aura_skills::SkillManager;
use aura_store::Store;
use aura_tools::automaton_tools::AutomatonController;
use aura_tools::domain_tools::DomainApi;
use aura_tools::{ToolCatalog, ToolConfig};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

// ============================================================================
// WebSocket Handler Context
// ============================================================================

/// Configuration passed to the WebSocket handler from the router state.
#[derive(Clone)]
pub(crate) struct WsContext {
    /// Default workspace base path.
    pub(crate) workspace_base: PathBuf,
    /// Shared model provider (type-erased).
    pub(crate) provider: Arc<dyn ModelProvider + Send + Sync>,
    /// Persistent store for kernel recording.
    ///
    /// TODO(phase2-followup): Invariant §10 wants this bound to
    /// `Arc<dyn ReadStore>`. The session itself never calls
    /// `append_entry_*`, but it hands the store to `Kernel::new`,
    /// which currently takes `Arc<dyn Store>`. Resolving this
    /// requires splitting the kernel constructor's store argument or
    /// introducing a `WriteHook` seam so the session can bind to the
    /// narrower read surface.
    pub(crate) store: Arc<dyn Store>,
    /// Local scheduler used for foreground subagent dispatch.
    pub(crate) scheduler: Arc<Scheduler>,
    /// Tool configuration (fs/cmd permissions).
    pub(crate) tool_config: ToolConfig,
    /// JWT auth token from the WebSocket upgrade request.
    pub(crate) auth_token: Option<String>,
    /// Canonical tool catalog (shared across sessions).
    pub(crate) catalog: Arc<ToolCatalog>,
    /// Domain API for native spec/task/project/orbit/network tool execution.
    pub(crate) domain_api: Option<Arc<dyn DomainApi>>,
    /// Automaton controller for dev-loop lifecycle (None when domain API unavailable).
    pub(crate) automaton_controller: Option<Arc<dyn AutomatonController>>,
    /// Optional project base for remapping project paths (from `AURA_PROJECT_BASE`).
    pub(crate) project_base: Option<PathBuf>,
    /// Optional memory manager for prompt injection and result ingestion.
    pub(crate) memory_manager: Option<Arc<aura_memory::MemoryManager>>,
    /// Optional skill manager for per-agent skill injection into prompts.
    pub(crate) skill_manager: Option<Arc<RwLock<SkillManager>>>,
    /// Router URL for generation proxying (from `AURA_ROUTER_URL`).
    pub(crate) router_url: Option<String>,
    /// aura-os-server base URL used by cross-agent callbacks.
    pub(crate) aura_os_server_url: Option<String>,
}

impl WsContext {
    /// Build a [`WsContext`] from a [`crate::gateway::RouterState`]
    /// snapshot plus the auth token resolved at the
    /// [`crate::gateway::RouterState`] entry point.
    pub(crate) fn from_state(
        state: &crate::gateway::RouterState,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            workspace_base: state.config.workspaces_path(),
            provider: state.provider.clone(),
            store: state.store.clone(),
            scheduler: state.scheduler.clone(),
            tool_config: state.tool_config.clone(),
            auth_token,
            catalog: state.catalog.clone(),
            domain_api: state.domain_api.clone(),
            automaton_controller: state.automaton_controller.clone(),
            project_base: state.config.project_base.clone(),
            memory_manager: state.memory_manager.clone(),
            skill_manager: state.skill_manager.clone(),
            router_url: state.router_url.clone(),
            aura_os_server_url: state.config.aura_os_server_url.clone(),
        }
    }
}
