//! Node runtime.

use crate::config::NodeConfig;
use crate::gateway::{create_router, RouterState};
use anyhow::Context;
use aura_agent::KernelModelGateway;
use aura_agent_kernel::{Executor, ExecutorRouter, Kernel, KernelConfig};
use aura_context_memory::{
    ConsolidationConfig, MemoryManager, ProcedureConfig, RefinerConfig, RetrievalConfig,
    WriteConfig,
};
use aura_context_skills::{SkillInstallStore, SkillLoader, SkillManager};
use aura_core_types::AgentId;
use aura_domain_http::HttpDomainApi;
use aura_engine::automaton::AutomatonBridge;
use aura_engine::scheduler::Scheduler;
use aura_store_db::RocksStore;
use aura_surface_automaton::AutomatonRuntime;
use aura_tools::automaton_tools::AutomatonController;
use aura_tools::catalog::ToolProfile;
use aura_tools::domain_tools::{DomainApi, DomainToolExecutor};
use aura_tools::{ToolCatalog, ToolConfig};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tracing::{info, warn};

/// The Aura Node runtime.
pub struct Node {
    config: NodeConfig,
}

impl Node {
    /// Create a new node with the given config.
    #[must_use]
    pub const fn new(config: NodeConfig) -> Self {
        Self { config }
    }

    /// Create a node with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(NodeConfig::default())
    }

    /// Run the node.
    ///
    /// # Errors
    /// Returns error if the node fails to start.
    pub async fn run(mut self) -> anyhow::Result<()> {
        info!("Starting Aura Node");
        info!(data_dir = ?self.config.data_dir, "Data directory");

        // Bind the TCP listener FIRST so port collisions fail in
        // milliseconds instead of after the full init sequence
        // (RocksDB open, tool catalog, domain API HTTP client,
        // memory manager, scheduler, automaton runtime, skill
        // loader). Pre-fix the bind was the last step on the happy
        // path, so a stale `aura-node.exe` orphaned from a previous
        // dev run would burn through ~1s of init logs only to fail
        // with a cryptic "Only one usage of each socket address"
        // error. Holding the listener across init also reserves the
        // port so a race between two startups can't both succeed
        // init only to have the later one die at the bind.
        let addr: SocketAddr = self
            .config
            .bind_addr
            .parse()
            .context("parsing bind address")?;
        let listener = bind_listener(addr).await?;

        let db_path = self.config.db_path();
        tokio::fs::create_dir_all(&db_path)
            .await
            .context("creating database directory")?;
        tokio::fs::create_dir_all(self.config.workspaces_path())
            .await
            .context("creating workspaces directory")?;

        // Security audit — phase 4. Resolve the bearer secret BEFORE we
        // build the router so every protected route sees the same
        // token. `resolve_auth_token` prefers `AURA_NODE_AUTH_TOKEN`,
        // then a persisted `$data_dir/auth_token` file, then mints a
        // new one (and prints it to stderr exactly once). See
        // `crate::config::resolve_auth_token` for the source-order
        // spec. The token is deliberately *not* logged via `tracing`.
        //
        // Gated on `require_auth` (AURA_NODE_REQUIRE_AUTH env) which
        // defaults to `false`; when disabled we clear the token rather
        // than leaving the `"test"` default in memory, so any code
        // path that accidentally compares against it fails closed.
        if self.config.require_auth {
            self.config.auth_token = crate::config::resolve_auth_token(&self.config.data_dir)
                .context("resolving aura-runtime auth token")?;
        } else {
            self.config.auth_token.clear();
        }

        // Swarm TEE phase 5 (attest-boot): when AURA_STATE_ENCRYPTION=sealed,
        // fetch the per-agent DEK from the CoCo CDH/KBS (or the dev-mode key
        // file) BEFORE opening or serving any state. This either returns a
        // value cipher or fails the whole startup — sealed mode never falls
        // back to plaintext. In plaintext mode (env unset) `seal_cipher` is
        // `None` and everything below behaves exactly as before.
        // R2: this also runs the encrypt-in-place migration when a legacy
        // agent's plaintext state is found at `db_path` on a sealed boot.
        let seal_cipher = crate::sealing::prepare_state_sealing(&self.config.data_dir, &db_path)
            .await
            .context("preparing sealed state mode (refusing to serve without the state DEK)")?;

        let store = Arc::new(
            RocksStore::open_sealed(&db_path, self.config.sync_writes, seal_cipher.clone())
                .context("opening RocksDB store")?,
        );
        info!(sealed = seal_cipher.is_some(), "Store opened");

        let mut tool_config = ToolConfig::for_autonomous_dev_loop();
        if self.config.allow_unrestricted_full_access {
            tool_config.command.allow_unrestricted_full_access = true;
            warn!("unrestricted full-access command allowlist bypass enabled by operator config");
        }
        if tool_config.command.enabled {
            info!(
                allowed_commands = ?tool_config.command.command_allowlist,
                binary_allowlist = ?tool_config.command.binary_allowlist,
                allow_shell = tool_config.command.allow_shell,
                allow_unrestricted_full_access = tool_config.command.allow_unrestricted_full_access,
                "aura-runtime run_command enabled"
            );
        }

        let catalog = Arc::new(ToolCatalog::new());
        info!(static_tools = catalog.static_count(), "Tool catalog ready");

        let domain_api: Arc<dyn DomainApi> = Arc::new(HttpDomainApi::new(
            &self.config.aura_storage_url,
            &self.config.aura_network_url,
            &self.config.orbit_url,
            self.config.aura_os_server_url.clone(),
        )?);
        info!(
            storage_url = %self.config.aura_storage_url,
            os_server_url = ?self.config.aura_os_server_url,
            "Domain API ready (JWT auth)"
        );

        let tools = catalog.visible_tools(ToolProfile::Core, &tool_config);
        let domain_exec = Arc::new(DomainToolExecutor::new(domain_api.clone()));
        let resolver =
            aura_engine::executor::build_tool_resolver(&catalog, &tool_config, Some(domain_exec));
        let resolver: Arc<dyn Executor> = Arc::new(resolver);
        let executors = vec![resolver];
        info!("Executors configured");

        let provider = aura_model_reasoner::default_provider_from_env()
            .context("building default model provider")?
            .provider;

        // Invariant §3: LLM calls performed by the memory subsystem are
        // recorded via a dedicated "memory service" kernel whose agent log
        // is kept distinct from per-user / per-session agent logs.
        let memory_agent_id = AgentId::generate();
        let memory_store: Arc<dyn aura_store_db::Store> = store.clone();
        let memory_kernel = Arc::new(
            Kernel::new(
                memory_store,
                provider.clone(),
                ExecutorRouter::new(),
                KernelConfig::default(),
                memory_agent_id,
            )
            .context("building memory-service kernel")?,
        );
        let memory_gateway = Arc::new(KernelModelGateway::new(memory_kernel));
        let memory_manager = Arc::new(MemoryManager::with_cipher(
            store.db_handle().clone(),
            seal_cipher.clone(),
            memory_gateway,
            RefinerConfig::default(),
            WriteConfig::default(),
            RetrievalConfig::default(),
            ConsolidationConfig::default(),
            ProcedureConfig::default(),
        ));
        info!(
            memory_agent_id = %memory_agent_id,
            "Memory manager ready"
        );

        let scheduler = Arc::new(Scheduler::new(
            store.clone(),
            provider.clone(),
            executors,
            tools,
            self.config.workspaces_path(),
            Some(Arc::clone(&memory_manager)),
        ));
        info!("Scheduler ready");

        let automaton_runtime = Arc::new(AutomatonRuntime::new());
        let automaton_bridge: Option<Arc<AutomatonBridge>> = Some(Arc::new(
            AutomatonBridge::new(
                automaton_runtime.clone(),
                store.clone() as Arc<dyn aura_store_db::Store>,
                domain_api.clone(),
                provider.clone(),
                catalog.clone(),
                tool_config.clone(),
            )
            .with_scheduler(scheduler.clone()),
        ));
        let automaton_controller: Option<Arc<dyn AutomatonController>> = automaton_bridge
            .clone()
            .map(|b| b as Arc<dyn AutomatonController>);
        if automaton_controller.is_some() {
            info!("Automaton runtime ready");
        }

        let skill_loader = SkillLoader::with_defaults(Some(self.config.workspaces_path()), None);
        let skill_install_store = Arc::new(SkillInstallStore::with_cipher(
            store.db_handle().clone(),
            seal_cipher.clone(),
        ));
        let skill_manager_inner =
            SkillManager::with_install_store(skill_loader, skill_install_store);
        let skill_count = skill_manager_inner.list_all().len();
        let skill_manager = Arc::new(RwLock::new(skill_manager_inner));
        info!(skills = skill_count, "Skill manager ready");

        let router_url = std::env::var("AURA_ROUTER_URL").ok();

        // In-TEE secrets vault (Swarm TEE phase 6): same shared DB +
        // optional state cipher as the memory/skill stores, so secrets
        // are sealed at rest whenever the node booted in sealed mode.
        let secrets_vault = Arc::new(aura_store_db::SecretsVault::with_cipher(
            store.db_handle().clone(),
            seal_cipher.clone(),
        ));

        // In-TEE process store (Swarm TEE phase 7): same shared DB +
        // optional state cipher, so process prompts/config and run
        // history are sealed at rest in sealed mode.
        let process_store = Arc::new(aura_store_db::ProcessStore::with_cipher(
            store.db_handle().clone(),
            seal_cipher.clone(),
        ));

        // Trigger-metadata registrar (Swarm TEE phase 8): pushes the
        // exportable (process_id, cron, enabled, next_run_at) set to
        // the swarm gateway after process mutations. Env-configured;
        // silently inert for local agents.
        let trigger_registrar = Arc::new(
            crate::trigger_registrar::TriggerRegistrar::from_env(process_store.clone()),
        );

        let state = RouterState::new(crate::gateway::RouterStateConfig {
            store,
            scheduler,
            config: self.config.clone(),
            provider,
            tool_config,
            catalog,
            domain_api: Some(domain_api),
            automaton_controller,
            automaton_bridge,
            memory_manager: Some(memory_manager),
            skill_manager: Some(skill_manager),
            secrets_vault: Some(secrets_vault),
            process_store: Some(process_store),
            trigger_registrar: Some(trigger_registrar),
            router_url,
        });
        let app = create_router(state);

        info!(%addr, "HTTP server listening");

        // `into_make_service_with_connect_info::<SocketAddr>()` is
        // required for the tower_governor `PeerIpKeyExtractor` layered
        // inside `create_router` (phase 9 rate limiting). Without it,
        // every request would be rejected with `UnableToExtractKey`.
        //
        // `with_graceful_shutdown(shutdown_signal())` is the other
        // half of the port-leak fix: pre-fix the server awaited
        // forever with no signal handler, so on Windows a Ctrl-C in
        // the foreground `cargo run` console did not propagate to
        // the child `aura-node.exe` (Windows console Ctrl-C
        // semantics are not the Unix signal-group propagation that
        // cargo relies on elsewhere), the child orphaned, and the
        // next `cargo run` collided on port 8080. Listening for
        // Ctrl-C / Ctrl-Break (Windows) and SIGTERM (Unix) lets
        // axum drain in-flight requests and close the listener so
        // the port releases as soon as the user stops the dev run.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;

        Ok(())
    }
}

/// Env var that disables the auto-kill self-heal on `AddrInUse`. Set
/// `AURA_NODE_DISABLE_PORT_SELF_HEAL=1` to force the bind to fail
/// immediately when the port is taken (useful when the operator
/// explicitly wants to run a second instance side-by-side on a
/// different port via `AURA_NODE_BIND_ADDR` and would rather get a
/// loud error than have `aura-node` reach over and kill the
/// already-running instance).
const DISABLE_SELF_HEAL_ENV: &str = "AURA_NODE_DISABLE_PORT_SELF_HEAL";

/// Bind the public HTTP listener with two layers of recovery on
/// `AddrInUse`:
///
/// 1. **Self-heal**: when the port is held by another process whose
///    image name starts with `aura-node` (i.e. an orphan from a
///    previous dev-run that did not exit cleanly — see
///    [`Node::run`]'s `with_graceful_shutdown` comment for why that
///    happens in the first place on Windows), kill it and retry the
///    bind exactly once. This automates the manual
///    `Get-Process aura-node* | Stop-Process -Force` workflow that
///    operators had to run on every collision pre-fix.
///
/// 2. **Actionable error**: if the holder is something other than an
///    `aura-node` process (so we deliberately do not kill it — the
///    user might intentionally have another service on 8080), fall
///    through to [`format_addr_in_use_error`] which names the
///    conflict, prints the kill one-liner the operator can run, and
///    suggests the `AURA_NODE_BIND_ADDR` escape hatch.
///
/// Self-heal is gated on [`DISABLE_SELF_HEAL_ENV`] so an operator who
/// is intentionally running two aura-nodes can disable the auto-kill
/// without losing the actionable error message.
async fn bind_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    match TcpListener::bind(addr).await {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if !self_heal_disabled() {
                match try_self_heal_orphan(addr).await {
                    Ok(true) => match TcpListener::bind(addr).await {
                        Ok(listener) => {
                            info!(%addr, "rebound after killing orphaned aura-node holder");
                            return Ok(listener);
                        }
                        Err(retry_err) => {
                            warn!(
                                %addr,
                                error = %retry_err,
                                "self-heal killed orphan but rebind still failed; reporting original error"
                            );
                        }
                    },
                    Ok(false) => {
                        // Holder is not an aura-node process, or we
                        // could not identify it — fall through to the
                        // actionable error below rather than killing
                        // an unrelated server.
                    }
                    Err(probe_err) => {
                        warn!(
                            %addr,
                            error = %probe_err,
                            "could not probe port holder for self-heal; falling through to actionable error"
                        );
                    }
                }
            }
            Err(anyhow::anyhow!("{}", format_addr_in_use_error(addr, &err)))
        }
        Err(err) => Err(anyhow::Error::new(err).context(format!("binding TCP listener on {addr}"))),
    }
}

fn self_heal_disabled() -> bool {
    std::env::var(DISABLE_SELF_HEAL_ENV)
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "True" | "TRUE" | "yes" | "on"))
}

/// Returns `Ok(true)` if the port holder was an `aura-node*` process
/// and was successfully killed; `Ok(false)` if the holder is not an
/// aura-node (so the caller should preserve the operator's other
/// service) or could not be identified; `Err` only for unexpected
/// failures probing the OS process table.
async fn try_self_heal_orphan(addr: SocketAddr) -> anyhow::Result<bool> {
    let Some((pid, image)) = find_port_holder(addr).await? else {
        return Ok(false);
    };
    if !is_aura_node_image(&image) {
        info!(
            %addr,
            holder_pid = pid,
            holder_image = %image,
            "port held by non-aura-node process; not self-healing"
        );
        return Ok(false);
    }
    warn!(
        %addr,
        orphan_pid = pid,
        orphan_image = %image,
        "self-healing: killing orphaned aura-node holder before retrying bind"
    );
    kill_pid(pid).await?;
    // Give the OS a beat to actually release the socket. Windows in
    // particular sometimes holds the port briefly after the owner
    // exits.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    Ok(true)
}

/// Predicate factored out for unit testing: any process whose image
/// name starts with `aura-node` is treated as one of "ours" for the
/// purpose of self-heal. Matches both `aura-node.exe` (the installed
/// or release binary) and cargo's `aura-node-0.1.0-<hash>.exe`
/// scratch builds that orphan most often during iterative dev.
fn is_aura_node_image(image: &str) -> bool {
    image.trim().to_ascii_lowercase().starts_with("aura-node")
}

#[cfg(windows)]
async fn find_port_holder(addr: SocketAddr) -> anyhow::Result<Option<(u32, String)>> {
    let needle = format!("{}:{}", addr.ip(), addr.port());
    let output = tokio::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .await
        .context("running netstat to probe port holder")?;
    if !output.status.success() {
        anyhow::bail!(
            "netstat exited with status {}",
            output.status.code().unwrap_or(-1)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid_opt = stdout
        .lines()
        .filter(|line| line.contains("LISTENING"))
        .filter(|line| line.contains(&needle))
        .filter_map(|line| line.split_whitespace().last())
        .filter_map(|tok| tok.parse::<u32>().ok())
        .next();
    let Some(pid) = pid_opt else {
        return Ok(None);
    };

    let image = tokio::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .await
        .context("running tasklist to identify port holder")?;
    if !image.status.success() {
        return Ok(Some((pid, String::new())));
    }
    let csv = String::from_utf8_lossy(&image.stdout);
    // tasklist CSV row: "Image Name","PID","Session Name","Session#","Mem Usage"
    let image_name = csv
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|line| line.split(',').next())
        .map(|raw| raw.trim().trim_matches('"').to_string())
        .unwrap_or_default();
    Ok(Some((pid, image_name)))
}

#[cfg(unix)]
async fn find_port_holder(addr: SocketAddr) -> anyhow::Result<Option<(u32, String)>> {
    let target = format!("{}:{}", addr.ip(), addr.port());
    let output = tokio::process::Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpc"])
        .output()
        .await
        .context("running lsof to probe port holder")?;
    if !output.status.success() {
        // lsof returns non-zero when nothing matches; treat as "no holder identified".
        return Ok(None);
    }
    // lsof -Fpc emits records like:
    //   p1234
    //   caura-node
    //   p5678
    //   ...
    // We need the (pid, image) tuple whose later TCP entry mentions our address;
    // simpler: run a second targeted query.
    let _ = output;
    let detail = tokio::process::Command::new("lsof")
        .args(["-tnP", &format!("-iTCP@{target}"), "-sTCP:LISTEN"])
        .output()
        .await
        .context("running targeted lsof for port holder")?;
    let pid_opt = String::from_utf8_lossy(&detail.stdout)
        .lines()
        .find_map(|l| l.trim().parse::<u32>().ok());
    let Some(pid) = pid_opt else {
        return Ok(None);
    };
    let comm = tokio::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .await
        .context("running ps to identify port holder image")?;
    let image_name = String::from_utf8_lossy(&comm.stdout).trim().to_string();
    Ok(Some((pid, image_name)))
}

#[cfg(not(any(unix, windows)))]
async fn find_port_holder(_addr: SocketAddr) -> anyhow::Result<Option<(u32, String)>> {
    Ok(None)
}

#[cfg(windows)]
async fn kill_pid(pid: u32) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .await
        .context("running taskkill")?;
    if !status.success() {
        anyhow::bail!(
            "taskkill exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn kill_pid(pid: u32) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .await
        .context("running kill -9")?;
    if !status.success() {
        anyhow::bail!("kill exited with status {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn kill_pid(_pid: u32) -> anyhow::Result<()> {
    anyhow::bail!("kill is not implemented on this platform")
}

/// Render the actionable `AddrInUse` message. Factored out so the
/// formatting can be unit-tested without a live `TcpListener`.
fn format_addr_in_use_error(addr: SocketAddr, err: &io::Error) -> String {
    let killer = if cfg!(windows) {
        "powershell -Command \"Get-Process -Name aura-node* -ErrorAction SilentlyContinue | Stop-Process -Force\""
    } else {
        "pkill -f aura-node"
    };
    format!(
        "binding TCP listener on {addr}: address already in use ({err}).\n\
         \n\
         A previous `aura-node` process is most likely still holding the port. \
         To kill it and retry:\n\
         \n  {killer}\n\n\
         Set `AURA_NODE_BIND_ADDR` to a different `host:port` if you want \
         to run a second instance side-by-side instead."
    )
}

/// Await the first OS shutdown signal and return. Resolves on Ctrl-C
/// everywhere; on Unix also resolves on `SIGTERM` (so `docker stop`
/// /  systemd `Stop=` close the listener cleanly); on Windows also
/// resolves on `Ctrl-Break`.
///
/// Awaiting any of these returns `()` so `axum::serve(...)
/// .with_graceful_shutdown(shutdown_signal())` drains in-flight
/// requests, closes the TCP listener, and lets `run` return `Ok(())`
/// instead of leaking the process across `cargo run` invocations.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!(error = %err, "failed to install Ctrl-C handler; graceful shutdown disabled");
            // Park forever so the `select!` falls through to whichever
            // platform-specific signal we did install.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                warn!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(windows)]
    let terminate = async {
        match tokio::signal::windows::ctrl_break() {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                warn!(error = %err, "failed to install Ctrl-Break handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(any(unix, windows)))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            info!("Ctrl-C received; shutting down HTTP server");
        }
        () = terminate => {
            info!("termination signal received; shutting down HTTP server");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_new() {
        let config = NodeConfig::default();
        let node = Node::new(config.clone());
        assert_eq!(node.config.bind_addr, config.bind_addr);
    }

    #[test]
    fn test_node_with_defaults() {
        let node = Node::with_defaults();
        assert_eq!(node.config.bind_addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_node_custom_config() {
        let config = NodeConfig {
            bind_addr: "0.0.0.0:9090".to_string(),
            sync_writes: true,
            record_window_size: 100,
            ..NodeConfig::default()
        };
        let node = Node::new(config);
        assert_eq!(node.config.bind_addr, "0.0.0.0:9090");
        assert!(node.config.sync_writes);
        assert_eq!(node.config.record_window_size, 100);
    }

    #[test]
    fn test_node_config_propagation() {
        let config = NodeConfig {
            data_dir: std::path::PathBuf::from("/custom/data"),
            ..NodeConfig::default()
        };
        let node = Node::new(config);
        assert_eq!(
            node.config.data_dir,
            std::path::PathBuf::from("/custom/data")
        );
    }

    #[test]
    fn test_create_model_provider_returns_something() {
        let _provider = aura_model_reasoner::default_provider_from_env()
            .expect("default provider should build");
    }

    /// `AddrInUse` errors must be reformatted into an actionable
    /// message that names the conflict, points the operator at the
    /// stale-`aura-node` killer command, and offers the
    /// `AURA_NODE_BIND_ADDR` escape hatch. Pinned because the
    /// recurring "stop didn't release the port" symptom is the whole
    /// reason this fail-fast path exists, and a regression that
    /// dropped the hint back to the raw `std::io::Error` would put
    /// us back in the world where the operator had to find the PID
    /// by hand every time.
    #[test]
    fn addr_in_use_error_includes_killer_and_bind_override() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("valid loopback addr");
        let err = io::Error::new(io::ErrorKind::AddrInUse, "address already in use (mock)");
        let message = super::format_addr_in_use_error(addr, &err);

        assert!(
            message.contains("127.0.0.1:8080"),
            "must name the conflicting address so the operator can find the right process; got `{message}`"
        );
        assert!(
            message.contains("AURA_NODE_BIND_ADDR"),
            "must surface the bind-override escape hatch; got `{message}`"
        );
        assert!(
            message.contains("aura-node"),
            "must mention `aura-node` so the killer command is recognisable; got `{message}`"
        );

        if cfg!(windows) {
            assert!(
                message.contains("Stop-Process"),
                "Windows hint must use PowerShell `Stop-Process`; got `{message}`"
            );
        } else {
            assert!(
                message.contains("pkill"),
                "non-Windows hint must use `pkill`; got `{message}`"
            );
        }
    }

    /// The self-heal is allowed to kill any process whose image name
    /// starts with `aura-node` — covering both the installed binary
    /// (`aura-node.exe`) and cargo's scratch builds
    /// (`aura-node-0.1.0-<hash>.exe`) that orphan most often. Pinned
    /// because narrowing this predicate (e.g. requiring an exact
    /// `aura-node.exe` match) would silently disable self-heal for
    /// the most common orphan source and put operators back in the
    /// "find PID by hand on every dev-run" loop.
    #[test]
    fn is_aura_node_image_matches_both_release_and_cargo_builds() {
        assert!(super::is_aura_node_image("aura-node.exe"));
        assert!(super::is_aura_node_image(
            "aura-node-0.1.0-23616512-abcdef.exe"
        ));
        assert!(super::is_aura_node_image("aura-node"));
        assert!(super::is_aura_node_image("AURA-NODE.EXE"));
        assert!(super::is_aura_node_image("  aura-node.exe  "));
    }

    /// Self-heal must NOT kill an unrelated server that happens to
    /// own port 8080 (nginx, another dev server the user actually
    /// wants, a sibling Aura repo's process from a different build,
    /// etc.). Anything that does not start with `aura-node` after
    /// case folding falls through to the actionable
    /// `format_addr_in_use_error` instead.
    #[test]
    fn is_aura_node_image_rejects_unrelated_processes() {
        assert!(!super::is_aura_node_image("nginx.exe"));
        assert!(!super::is_aura_node_image("node.exe"));
        assert!(!super::is_aura_node_image("aura-os-server.exe"));
        assert!(!super::is_aura_node_image("aura-runtime.exe"));
        assert!(!super::is_aura_node_image(""));
        assert!(!super::is_aura_node_image("python.exe"));
    }

    /// The `AURA_NODE_DISABLE_PORT_SELF_HEAL` escape hatch must
    /// accept the standard truthy spellings so an operator running
    /// two intentional aura-node instances can opt out of the
    /// auto-kill without having to remember a single magic value.
    /// Pinned because forgetting to handle one of these would silently
    /// re-enable self-heal for that operator and surprise-kill their
    /// other instance.
    #[test]
    fn self_heal_disabled_recognises_truthy_env_spellings() {
        let key = super::DISABLE_SELF_HEAL_ENV;
        let prev = std::env::var(key).ok();
        let with_env = |val: Option<&str>, f: &dyn Fn()| {
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            f();
        };

        with_env(None, &|| assert!(!super::self_heal_disabled()));
        with_env(Some(""), &|| assert!(!super::self_heal_disabled()));
        with_env(Some("0"), &|| assert!(!super::self_heal_disabled()));
        with_env(Some("false"), &|| assert!(!super::self_heal_disabled()));

        for truthy in ["1", "true", "True", "TRUE", "yes", "on"] {
            with_env(Some(truthy), &|| {
                assert!(
                    super::self_heal_disabled(),
                    "`{truthy}` must be recognised as a truthy disable signal"
                );
            });
        }

        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
