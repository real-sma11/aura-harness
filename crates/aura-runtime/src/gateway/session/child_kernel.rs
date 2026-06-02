//! Runtime-side implementation of
//! [`aura_engine::child_kernel::ChildKernelFactory`].
//!
//! This is the upper half of the cross-crate seam that lets a child
//! subagent run reuse the real-agent executor router instead of the
//! scheduler's bare node-level resolver. The trait is declared in
//! `aura-engine` (a lower layer); this implementation assembles a
//! session-equivalent [`ExecutorRouter`] from the pieces captured off
//! the parent's [`WsContext`] + [`Session`] at kernel-build time.
//!
//! ## Breaking the construction cycle
//!
//! A child's resolver itself needs a subagent-dispatch hook (so the
//! child can spawn *its own* children), and that hook needs a
//! [`RuntimeChildRunner`], which needs *this* factory again. The
//! factory is therefore self-referential: it is built with
//! [`Arc::new_cyclic`] and stores a [`Weak`] handle to itself, which it
//! upgrades and re-injects into every child runner it constructs. That
//! makes the session resolver available to children, grandchildren, and
//! deeper without any upward dependency from `aura-engine`.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use aura_agent_kernel::{ExecutorRouter, KernelSpawnHook};
use aura_agent_subagent::SubagentRegistry;
use aura_core_types::InstalledToolDefinition;
use aura_engine::child_kernel::{ChildKernelFactory, ChildKernelRequest};
use aura_engine::child_runner::RuntimeChildRunner;
use aura_engine::executor;
use aura_engine::scheduler::Scheduler;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{ChildRunner, OrphanStore, ParentLeaseRegistry};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_store_db::Store;
use aura_tools::automaton_tools::AutomatonController;
use aura_tools::domain_tools::DomainToolExecutor;
use aura_tools::SubagentDispatchHook;

/// Captured devloop-control wiring so child runs expose the same
/// automaton tools the parent session does.
struct AutomatonToolParams {
    controller: Arc<dyn AutomatonController>,
    project_id: String,
    workspace_root: Option<PathBuf>,
    auth_token: Option<String>,
}

/// Session-scoped [`ChildKernelFactory`]. Captures the immutable
/// session surface (catalog, tool config, hooks wiring, store /
/// scheduler / registry handles, workspace root) once, then stamps out
/// a session-equivalent [`ExecutorRouter`] per child run.
pub(crate) struct SessionChildKernelFactory {
    me: Weak<SessionChildKernelFactory>,
    catalog: Arc<aura_tools::ToolCatalog>,
    session_tool_config: aura_tools::ToolConfig,
    domain_exec: Option<Arc<DomainToolExecutor>>,
    installed_tools: Vec<InstalledToolDefinition>,
    automaton: Option<AutomatonToolParams>,
    store: Arc<dyn Store>,
    scheduler: Arc<Scheduler>,
    subagent_registry: SubagentRegistry,
    orphan_dir: PathBuf,
    workspace: PathBuf,
    use_workspace_base_as_root: bool,
    aura_os_server_url: Option<String>,
    auth_token: Option<String>,
    aura_org_id: Option<String>,
}

/// Inputs for [`SessionChildKernelFactory::new`], mirroring the
/// session/context state `build_kernel_with_config` already resolved.
pub(crate) struct SessionChildKernelFactoryParams {
    pub catalog: Arc<aura_tools::ToolCatalog>,
    pub session_tool_config: aura_tools::ToolConfig,
    pub domain_exec: Option<Arc<DomainToolExecutor>>,
    pub installed_tools: Vec<InstalledToolDefinition>,
    pub automaton_controller: Option<Arc<dyn AutomatonController>>,
    pub automaton_project_id: String,
    pub automaton_workspace_root: Option<PathBuf>,
    pub store: Arc<dyn Store>,
    pub scheduler: Arc<Scheduler>,
    pub subagent_registry: SubagentRegistry,
    pub orphan_dir: PathBuf,
    pub workspace: PathBuf,
    pub use_workspace_base_as_root: bool,
    pub aura_os_server_url: Option<String>,
    pub auth_token: Option<String>,
    pub aura_org_id: Option<String>,
}

impl SessionChildKernelFactory {
    /// Build the self-referential factory. The returned `Arc` is what
    /// gets injected into the parent's [`RuntimeChildRunner`].
    pub(crate) fn new(params: SessionChildKernelFactoryParams) -> Arc<Self> {
        let automaton = params
            .automaton_controller
            .map(|controller| AutomatonToolParams {
                controller,
                project_id: params.automaton_project_id,
                workspace_root: params.automaton_workspace_root,
                auth_token: params.auth_token.clone(),
            });
        Arc::new_cyclic(|me| SessionChildKernelFactory {
            me: me.clone(),
            catalog: params.catalog,
            session_tool_config: params.session_tool_config,
            domain_exec: params.domain_exec,
            installed_tools: params.installed_tools,
            automaton,
            store: params.store,
            scheduler: params.scheduler,
            subagent_registry: params.subagent_registry,
            orphan_dir: params.orphan_dir,
            workspace: params.workspace,
            use_workspace_base_as_root: params.use_workspace_base_as_root,
            aura_os_server_url: params.aura_os_server_url,
            auth_token: params.auth_token,
            aura_org_id: params.aura_org_id,
        })
    }

    /// Construct the subagent-dispatch hook for a child run. Each child
    /// gets fresh fleet primitives (registry / quota / leases / orphan
    /// store) and a child runner that re-injects this same factory, so
    /// the child's own spawns reuse the session resolver too.
    fn child_dispatch_hook(&self) -> Arc<dyn SubagentDispatchHook> {
        let child_runner: Arc<dyn ChildRunner> = {
            let mut runner = RuntimeChildRunner::new(
                self.store.clone(),
                self.scheduler.clone(),
                self.subagent_registry.clone(),
            )
            .with_child_workspace(self.workspace.clone(), self.use_workspace_base_as_root);
            if let Some(factory) = self.me.upgrade() {
                runner = runner.with_child_kernel_factory(factory);
            }
            Arc::new(runner)
        };
        Arc::new(FleetSubagentDispatcher::with_components(
            self.store.clone(),
            self.subagent_registry.clone(),
            Arc::new(FleetRegistry::new()),
            Arc::new(QuotaPool::new()),
            Arc::new(ParentLeaseRegistry::new()),
            Arc::new(OrphanStore::new(self.orphan_dir.clone())),
            child_runner,
        ))
    }
}

impl ChildKernelFactory for SessionChildKernelFactory {
    fn build_child_router(&self, request: ChildKernelRequest) -> ExecutorRouter {
        let mut resolver = executor::build_tool_resolver(
            &self.catalog,
            &self.session_tool_config,
            self.domain_exec.clone(),
        )
        .with_installed_tools(self.installed_tools.clone());

        if let Some(ref automaton) = self.automaton {
            for tool in aura_tools::automaton_tools::devloop_control_tools(
                automaton.controller.clone(),
                automaton.project_id.clone(),
                automaton.workspace_root.clone(),
                automaton.auth_token.clone(),
            ) {
                resolver.register(tool);
            }
        }

        resolver = resolver.with_subagent_dispatch_hook(self.child_dispatch_hook());

        // Same spawn-hook selection the session build performs: prefer
        // the aura-os-server cross-agent hook when a base URL is
        // configured, otherwise the local kernel spawn hook.
        if let Some(base_url) = self
            .aura_os_server_url
            .as_deref()
            .filter(|url| !url.is_empty())
        {
            resolver = resolver.with_spawn_hook(Arc::new(
                super::cross_agent_hook::AuraServerSpawnHook::new(
                    base_url.to_string(),
                    self.auth_token.clone(),
                    self.aura_org_id.clone(),
                    self.store.clone(),
                ),
            ));
            let hook = Arc::new(super::cross_agent_hook::AuraServerAgentHook::new(
                base_url.to_string(),
                self.auth_token.clone(),
            ));
            resolver = resolver
                .with_agent_control_hook(hook.clone())
                .with_agent_read_hook(hook);
        } else {
            resolver = resolver.with_spawn_hook(Arc::new(KernelSpawnHook::new(self.store.clone())));
        }

        resolver = resolver
            .with_caller_permissions(request.permissions)
            .with_tool_permission_context(
                request.user_tool_defaults,
                Some(request.tool_permissions),
            )
            .with_parent_chain(request.parent_chain);
        if let Some(user_id) = request.originating_user_id {
            if !user_id.trim().is_empty() {
                resolver = resolver.with_originating_user_id(user_id);
            }
        }
        if !request.model_id.trim().is_empty() {
            resolver = resolver.with_caller_model_id(request.model_id);
        }

        executor::build_executor_router(resolver)
    }
}
