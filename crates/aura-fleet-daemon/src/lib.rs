//! # aura-fleet-daemon
//!
//! Layer: fleet
//!
//! Composition root for the fleet layer. Phase 7a wires
//! [`FleetRegistry`], [`FleetSpawner`], [`FleetDispatcher`], and
//! [`QuotaPool`] into a single [`FleetDaemon`] holder so the surface
//! crates (and today's `aura-runtime` adapter) can grab an
//! `Arc<FleetDaemon>` and reach into any of the four subsystems
//! through [`FleetDaemon::handle`].
//!
//! Phase 7b grows this crate substantially: it will own the event
//! loop, the mailbox router, the plugin materialisation seam, and
//! the session supervisor. Phase 7a's [`FleetDaemon::run`] is a
//! deliberate no-op shell — calling it is harmless and the daemon
//! becomes useful through the typed handle accessors.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - All Arc handles are constructed inside
//!   [`FleetDaemon::builder`] / [`FleetDaemon::new`] so there is a
//!   single deterministic source for each subsystem. Surface crates
//!   may NOT new up their own [`FleetSpawner`] / [`FleetRegistry`] /
//!   etc. — that would defeat the cross-spawn invariants (e.g. a
//!   second [`FleetSpawner`] with its own [`ParentLeaseRegistry`]
//!   would not serialise spawns from the same parent against the
//!   first spawner's leases).
//! - Construction is failable only via the optional `try_*`
//!   helpers Phase 7b adds; Phase 7a's `new` cannot fail since it
//!   does no I/O.
//!
//! ## Assumptions
//!
//! - The caller owns a `tokio::runtime::Runtime` (library crates
//!   never construct one — see plan §2 cross-cutting ownership).
//! - The caller supplies a concrete `Arc<dyn Store>` and a concrete
//!   `Arc<dyn ChildRunner>` because both ultimately live in the
//!   surface / runtime layer and the daemon does not know how to
//!   build either itself.
//!
//! ## Failure modes
//!
//! - [`FleetDaemonError::NoOpShell`] — placeholder variant kept
//!   non-empty so the closed-enum gains forward-compat headroom
//!   without churn when Phase 7b adds real variants.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_config::PluginsConfig;
use aura_context_skills::SkillRegistry;
use aura_fleet_dispatch::FleetDispatcher;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore, ParentLeaseRegistry,
};
use aura_plugin_core::{load_enabled_plugins, PluginRuntime};
use aura_plugin_hooks::{CtxMeta, HookEngine, HookEvent, SessionStartHookCtx};
use aura_store::Store;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::info;

pub use aura_fleet_dispatch::AgentJob;
pub use aura_fleet_mailbox::{Mailbox, MailboxConfig, MailboxError, MailboxSender};
pub use aura_plugin_core::PluginLoadError;

/// Wiring config consumed at daemon construction time.
#[derive(Debug, Clone, Default)]
pub struct DaemonConfig {
    /// Per-spawn config forwarded to the [`FleetSpawner`].
    pub spawner: FleetSpawnerConfig,
    /// Mailbox capacity / backpressure config.
    pub mailbox: MailboxConfig,
    /// Override the orphan store root. `None` uses
    /// [`OrphanStore::default_root`] (`~/.aura/state/orphans/`).
    pub orphan_root: Option<PathBuf>,
}

/// Errors surfaced by [`FleetDaemon`] APIs.
#[derive(Debug, Error)]
pub enum FleetDaemonError {
    /// The mailbox receiver has already been claimed by a prior
    /// [`FleetDaemon::run`] call.
    #[error("fleet daemon: mailbox receiver already taken")]
    ReceiverAlreadyTaken,
    /// Catastrophic plugin load failure (the plugins root exists
    /// but cannot be walked). Per-plugin failures do NOT surface
    /// here — they're captured in
    /// [`SessionStartReport::plugin_load_failures`].
    #[error("fleet daemon: plugin load failed: {0}")]
    Plugin(String),
}

/// Report returned from [`FleetDaemon::prepare_session`].
///
/// The materialised [`PluginRuntime`] is exposed so the caller can
/// clone its `Arc<HookEngine>` / `Arc<McpConnectionManager>` /
/// `Arc<ConnectorRegistry>` handles into the agent loop and fleet
/// spawner config.
#[derive(Debug)]
pub struct SessionStartReport {
    /// Materialised plugin runtime. The hook engine is already
    /// loaded with every enabled plugin's hook contributions; the
    /// `SessionStart` event has already fired for every registered
    /// handler.
    pub runtime: PluginRuntime,
    /// Echoed session id (for callers that use the report to
    /// derive subsequent firing-site contexts).
    pub session_id: String,
    /// Echoed root agent id.
    pub agent_id: String,
}

impl SessionStartReport {
    /// Convenience accessor for the per-plugin failure list. Returns
    /// the same `Vec` as `runtime.load_failures` — kept here as a
    /// shortcut so callers don't need to chase through the runtime
    /// field.
    #[must_use]
    pub fn plugin_load_failures(&self) -> &[aura_plugin_hooks::PluginLoadFailure] {
        &self.runtime.load_failures
    }
}

/// Read-only bundle of [`Arc`] handles to the daemon's subsystems.
/// Cheap to clone — each field is a single `Arc`.
#[derive(Clone)]
pub struct FleetDaemonHandle {
    registry: Arc<FleetRegistry>,
    spawner: Arc<FleetSpawner>,
    dispatcher: Arc<FleetDispatcher>,
    quota: Arc<QuotaPool>,
    leases: Arc<ParentLeaseRegistry>,
    orphans: Arc<OrphanStore>,
    mailbox_sender: MailboxSender,
}

impl FleetDaemonHandle {
    /// Shared [`FleetRegistry`] handle.
    #[must_use]
    pub fn registry(&self) -> Arc<FleetRegistry> {
        self.registry.clone()
    }

    /// Shared [`FleetSpawner`] handle.
    #[must_use]
    pub fn spawner(&self) -> Arc<FleetSpawner> {
        self.spawner.clone()
    }

    /// Shared [`FleetDispatcher`] handle.
    #[must_use]
    pub fn dispatcher(&self) -> Arc<FleetDispatcher> {
        self.dispatcher.clone()
    }

    /// Shared [`QuotaPool`] handle.
    #[must_use]
    pub fn quota(&self) -> Arc<QuotaPool> {
        self.quota.clone()
    }

    /// Shared [`ParentLeaseRegistry`] handle. Surface crates that
    /// need to peek at the lease pool for observability use this;
    /// the actual acquire/release flow stays inside
    /// [`FleetSpawner`].
    #[must_use]
    pub fn leases(&self) -> Arc<ParentLeaseRegistry> {
        self.leases.clone()
    }

    /// Shared [`OrphanStore`] handle. Used by `aura agents
    /// inspect/reap` to list and clean up detached / abandoned
    /// children.
    #[must_use]
    pub fn orphans(&self) -> Arc<OrphanStore> {
        self.orphans.clone()
    }

    /// Mailbox sender — clone via [`MailboxSender::clone`] and
    /// hand to producers (task tool, SDK, RPC surface).
    #[must_use]
    pub fn mailbox_sender(&self) -> MailboxSender {
        self.mailbox_sender.clone()
    }
}

/// Phase 7b composition root. Holds owned `Arc`s to the fleet
/// subsystems plus the shared dispatcher and the mailbox receiver
/// driven by [`FleetDaemon::run`]. Surface code reads handles via
/// [`FleetDaemon::handle`].
pub struct FleetDaemon {
    handle: FleetDaemonHandle,
    config: DaemonConfig,
    receiver: Mutex<Option<aura_fleet_mailbox::MailboxReceiver>>,
}

impl FleetDaemon {
    /// Construct a fully-wired daemon.
    ///
    /// The caller supplies the store + child runner because both
    /// types are runtime-side (the daemon crate cannot synthesise
    /// either without depending upward on `aura-runtime`).
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        child_runner: Arc<dyn ChildRunner>,
        config: DaemonConfig,
    ) -> Self {
        let registry = Arc::new(FleetRegistry::new());
        let quota = Arc::new(QuotaPool::new());
        let leases = Arc::new(ParentLeaseRegistry::new());
        let orphan_root = config
            .orphan_root
            .clone()
            .unwrap_or_else(|| OrphanStore::default_root().unwrap_or_else(|_| PathBuf::from(".")));
        let orphans = Arc::new(OrphanStore::new(orphan_root));
        let spawner = Arc::new(FleetSpawner::with_default_derivation(
            store,
            registry.clone(),
            quota.clone(),
            leases.clone(),
            orphans.clone(),
            child_runner,
            config.spawner.clone(),
        ));
        let dispatcher = Arc::new(FleetDispatcher::new(spawner.clone()));
        let mailbox = Mailbox::with_config(config.mailbox);
        let (mailbox_sender, mailbox_receiver) = mailbox.into_parts();
        info!("fleet daemon: subsystems wired (Phase 7b)");
        Self {
            handle: FleetDaemonHandle {
                registry,
                spawner,
                dispatcher,
                quota,
                leases,
                orphans,
                mailbox_sender,
            },
            config,
            receiver: Mutex::new(Some(mailbox_receiver)),
        }
    }

    /// Cheap-clone handle to the daemon's subsystems.
    #[must_use]
    pub fn handle(&self) -> FleetDaemonHandle {
        self.handle.clone()
    }

    /// Read-only access to the resolved daemon config.
    #[must_use]
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// **Phase 8** session-start materialisation.
    ///
    /// Walks `aura_home/plugins/` (filtered by `plugins_config`),
    /// materialises the enabled plugins into a [`PluginRuntime`],
    /// merges plugin skill roots into `skills`, and fires
    /// [`HookEvent::SessionStart`] against the freshly-loaded hook
    /// engine.
    ///
    /// Returns a [`SessionStartReport`] that the caller can inspect
    /// (or surface into a session-start record). The report
    /// includes the materialised [`PluginRuntime`] (clone the
    /// `Arc` handles to wire the engine into your agent loop /
    /// fleet spawner).
    ///
    /// **Backward-compat invariant**: an empty `~/.aura/plugins/`
    /// directory + an empty `[plugins]` config yield a zero-cost
    /// pass-through:
    /// - `runtime.is_empty() == true`
    /// - the [`HookEngine`] short-circuits via `is_empty(event)`
    ///   on every subsequent firing site.
    /// - no skill roots are added, no MCP processes are spawned.
    ///
    /// # Errors
    ///
    /// Returns [`FleetDaemonError::Plugin`] for catastrophic walk
    /// failures (the plugins directory exists but is unreadable).
    /// Per-plugin failures are collected into
    /// [`SessionStartReport::plugin_load_failures`] and do NOT
    /// fail this call.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_session(
        &self,
        aura_home: &Path,
        plugins_config: &PluginsConfig,
        skills: &mut SkillRegistry,
        agent_id: &str,
        session_id: &str,
        mode: &str,
        model_id: &str,
    ) -> Result<SessionStartReport, FleetDaemonError> {
        let runtime = load_enabled_plugins(aura_home, plugins_config)
            .map_err(|err| FleetDaemonError::Plugin(format!("plugin load: {err}")))?;

        // Skill roots: hand to the skills registry.
        skills.add_plugin_roots(&runtime.skill_roots);

        // SessionStart hook ctx → fire chain.
        let ctx = SessionStartHookCtx {
            meta: CtxMeta {
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                parent_agent_id: None,
            },
            mode: mode.to_string(),
            model_id: model_id.to_string(),
            enabled_plugins: runtime.enabled.clone(),
            plugin_load_failures: runtime.load_failures.clone(),
        };
        if !runtime.hook_engine.is_empty(HookEvent::SessionStart) {
            let _ = HookEngine::fire_event(&runtime.hook_engine, &ctx, aura_home);
        }

        info!(
            session_id,
            agent_id,
            enabled_count = runtime.enabled.len(),
            failure_count = runtime.load_failures.len(),
            skill_root_count = runtime.skill_roots.len(),
            "fleet daemon: session prepared"
        );

        Ok(SessionStartReport {
            runtime,
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
        })
    }

    /// Phase 7b: drain the mailbox until every sender is dropped.
    /// Each [`AgentJob`] dequeued is routed through the shared
    /// [`FleetDispatcher::spawn_one`].
    ///
    /// Calling `run` a second time after the first run consumed the
    /// receiver returns [`FleetDaemonError::ReceiverAlreadyTaken`].
    ///
    /// # Errors
    ///
    /// See [`FleetDaemonError`].
    pub async fn run(&self) -> Result<(), FleetDaemonError> {
        let mut receiver = self
            .receiver
            .lock()
            .await
            .take()
            .ok_or(FleetDaemonError::ReceiverAlreadyTaken)?;
        let dispatcher = self.handle.dispatcher();
        info!("fleet daemon: mailbox loop entered");
        while let Some(job) = receiver.recv().await {
            if let Err(err) = dispatcher.spawn_one(job).await {
                tracing::warn!(error = %err, "fleet daemon: dispatch error");
            }
        }
        info!("fleet daemon: mailbox drained — exiting");
        Ok(())
    }
}
