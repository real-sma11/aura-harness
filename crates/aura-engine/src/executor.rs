//! Shared construction helpers for `ToolResolver` and `ExecutorRouter`.

use aura_agent_kernel::ExecutorRouter;
use aura_exec_runner::{ToolCatalog, ToolConfig, ToolResolver};
use aura_tools::domain_tools::DomainToolExecutor;
use std::sync::Arc;

/// Build a [`ToolResolver`] over the shared catalog + tool config,
/// optionally wiring a domain executor for `aura-os-server` domain
/// tools (specs, tasks, etc.).
pub fn build_tool_resolver(
    catalog: &Arc<ToolCatalog>,
    tool_config: &ToolConfig,
    domain_exec: Option<Arc<DomainToolExecutor>>,
) -> ToolResolver {
    let mut resolver = ToolResolver::new(catalog.clone(), tool_config.clone());
    if let Some(exec) = domain_exec {
        resolver = resolver.with_domain_executor(exec);
    }
    resolver
}

/// Wrap a [`ToolResolver`] in an [`ExecutorRouter`] so it can be handed
/// to [`aura_agent_kernel::Kernel::new`].
pub fn build_executor_router(resolver: ToolResolver) -> ExecutorRouter {
    let mut router = ExecutorRouter::new();
    router.add_executor(Arc::new(resolver));
    router
}
