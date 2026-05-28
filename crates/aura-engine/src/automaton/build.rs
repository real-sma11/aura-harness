//! Per-agent kernel construction for automaton orchestration.
//!
//! `start_dev_loop_with_capabilities` and `run_task_with_capabilities`
//! both need the same kernel-building dance: filter the installed-tool
//! list against installed integrations, wire a `DomainToolExecutor`
//! and a tool resolver, build a per-agent policy, then construct the
//! `Kernel` (with retry-on-failure to avoid panicking the node
//! process). That logic is gathered here so the dispatch entry-points
//! can stay focused on the lifecycle dance (install automaton,
//! record lifecycle event, spawn event forwarder).

use std::sync::Arc;

use aura_core::{
    installed_integrations_satisfy, AgentId, AgentPermissions, InstalledIntegrationDefinition,
    InstalledToolDefinition,
};
use aura_kernel::{Kernel, KernelConfig, PolicyConfig};
use aura_tools::domain_tools::{DomainApi, DomainToolExecutor};
use tracing::warn;

use crate::executor;

use super::AutomatonBridge;

impl AutomatonBridge {
    pub(super) fn prepare_installed_tools(
        installed_tools: Option<Vec<aura_protocol::InstalledTool>>,
        installed_integrations: &[InstalledIntegrationDefinition],
    ) -> Vec<InstalledToolDefinition> {
        installed_tools
            .unwrap_or_default()
            .into_iter()
            .map(aura_protocol::installed_tool_to_core)
            .filter(|tool| match tool.required_integration.as_ref() {
                Some(req) => installed_integrations_satisfy(req, installed_integrations),
                None => true,
            })
            .collect()
    }

    /// Build a per-agent [`Kernel`] backed by the shared store.
    ///
    /// The returned kernel owns an `ExecutorRouter` wired to the domain API
    /// (with optional JWT + project context) and serves as the single authority
    /// for tool execution and model reasoning recording for this agent.
    ///
    /// Returns an error string (propagated verbatim to callers that use
    /// `Result<String, String>`) when every retry of `Kernel::new` fails. The
    /// previous implementation panicked via `unreachable!` on exhaustion; a
    /// panic here would take down the node process, so we surface the final
    /// error instead and let the caller convert it into an `Err` response.
    #[allow(clippy::too_many_arguments)] // TODO(W4): group inputs into a `BuildKernelParams` struct.
    pub(super) fn build_kernel(
        &self,
        domain: Arc<dyn DomainApi>,
        auth_token: Option<&str>,
        project_id: Option<&str>,
        workspace: &std::path::Path,
        use_workspace_base_as_root: bool,
        installed_tools: Vec<InstalledToolDefinition>,
        installed_integrations: Vec<InstalledIntegrationDefinition>,
        agent_permissions: AgentPermissions,
    ) -> Result<Arc<Kernel>, String> {
        let domain_exec = Arc::new(DomainToolExecutor::with_session_context(
            domain,
            auth_token.map(String::from),
            project_id.map(String::from),
            Some(workspace.to_string_lossy().into_owned()),
        ));
        let resolver = executor::build_tool_resolver(
            &self.catalog,
            &self.tool_config,
            Some(domain_exec.clone()),
        )
        .with_installed_tools(installed_tools.clone());
        let router = executor::build_executor_router(resolver);
        let agent_id = AgentId::generate();
        let policy = automaton_policy_config(
            &installed_tools,
            &installed_integrations,
            agent_permissions.clone(),
        );
        let config = KernelConfig {
            workspace_base: workspace.to_path_buf(),
            use_workspace_base_as_root,
            policy,
            ..KernelConfig::default()
        };

        match Kernel::new(
            self.store.clone(),
            self.provider.clone(),
            router,
            config,
            agent_id,
        ) {
            Ok(k) => Ok(Arc::new(k)),
            Err(e) => {
                warn!(error = %e, "Kernel::new failed, falling back to fresh agent id");
                let fallback_router = executor::build_executor_router(
                    executor::build_tool_resolver(&self.catalog, &self.tool_config, None)
                        .with_installed_tools(installed_tools.clone()),
                );
                // Retry with a fresh `AgentId` and the same config; the only
                // failure mode left for `Kernel::new` is store corruption, in
                // which case we log and fall through to a second attempt.
                match Kernel::new(
                    self.store.clone(),
                    self.provider.clone(),
                    fallback_router,
                    KernelConfig {
                        workspace_base: workspace.to_path_buf(),
                        use_workspace_base_as_root,
                        policy: automaton_policy_config(
                            &installed_tools,
                            &installed_integrations,
                            agent_permissions,
                        ),
                        ..KernelConfig::default()
                    },
                    AgentId::generate(),
                ) {
                    Ok(k) => Ok(Arc::new(k)),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "fallback Kernel::new failed; dev-loop will be unavailable for this project"
                        );
                        // Final-resort path: re-run `Kernel::new` with the
                        // already-validated router and the minimum viable
                        // config. If this also fails we surface the error to
                        // the caller instead of panicking the node process.
                        let last_resort = executor::build_executor_router(
                            executor::build_tool_resolver(&self.catalog, &self.tool_config, None),
                        );
                        match Kernel::new(
                            self.store.clone(),
                            self.provider.clone(),
                            last_resort,
                            KernelConfig::default(),
                            AgentId::generate(),
                        ) {
                            Ok(k) => Ok(Arc::new(k)),
                            Err(final_err) => Err(format!(
                                "Kernel::new failed on default config after two retries: {final_err}"
                            )),
                        }
                    }
                }
            }
        }
    }
}

/// Build automaton kernel policy.
///
/// Tool availability now comes from the persisted user defaults and optional
/// agent overrides on [`PolicyConfig`]. This helper only wires runtime
/// integration requirements for installed tools.
fn automaton_policy_config(
    installed_tools: &[InstalledToolDefinition],
    installed_integrations: &[InstalledIntegrationDefinition],
    agent_permissions: AgentPermissions,
) -> PolicyConfig {
    let mut policy = PolicyConfig::default().with_agent_permissions(agent_permissions);
    policy.set_installed_integrations(installed_integrations.iter().cloned());
    policy.set_tool_integration_requirements(installed_tools.iter().filter_map(|tool| {
        tool.required_integration
            .clone()
            .map(|requirement| (tool.name.clone(), requirement))
    }));
    policy
}

#[cfg(test)]
mod tests {
    use aura_core::{AgentPermissions, Capability};

    use super::automaton_policy_config;

    #[test]
    fn automaton_policy_carries_agent_permissions() {
        let permissions = AgentPermissions {
            scope: Default::default(),
            capabilities: vec![Capability::InvokeProcess],
        };

        let policy = automaton_policy_config(&[], &[], permissions.clone());

        assert_eq!(policy.agent_permissions, permissions);
    }
}
