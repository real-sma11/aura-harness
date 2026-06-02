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
use std::time::Duration;

use aura_config::{FleetConfig, PluginsConfig};
use aura_context_skills::SkillRegistry;
use aura_core_modes::AgentMode;
use aura_core_types::{AgentId, Transaction, TransactionType};
use aura_fleet_dispatch::FleetDispatcher;
use aura_fleet_quota::QuotaPool;
use aura_fleet_registry::FleetRegistry;
use aura_fleet_spawn::{
    ChildRunner, FleetSpawner, FleetSpawnerConfig, OrphanStore, ParentLeaseRegistry,
};
use aura_plugin_core::{load_enabled_plugins, PluginRuntime};
use aura_plugin_hooks::{CtxMeta, HookEngine, HookEvent, SessionStartHookCtx};
use aura_store_db::Store;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Default wall-clock grace period applied during
/// [`FleetDaemon::run`] cooperative shutdown.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Phase 10 carve-out 2 — on-wire payload shape of the
/// [`aura_store_record::RecordKind::SessionStop`] audit record.
///
/// The struct lives here (the fleet-layer crate) rather than in
/// `aura-store-record` because the fleet daemon is the sole
/// producer; the store crate continues to own only the closed
/// `RecordKind` taxonomy. Consumers wishing to parse the on-disk
/// shape can deserialise the [`Transaction::payload`] bytes back
/// into [`SessionStopRecordPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStopRecordPayload {
    /// Stable on-disk discriminator string; pinned to
    /// `"session_stop"` to make Phase 7a / Phase 10 audit
    /// consumers' string-match path unambiguous even if a future
    /// schema bump retitles the wrapping `RecordKind` enum.
    pub kind: String,
    /// Session identifier supplied by the surface caller (e.g.
    /// the WebSocket session id, or the TUI session uuid).
    pub session_id: String,
    /// Root agent for the session (hex-encoded).
    pub agent_id: String,
    /// Total iterations (model calls) consumed.
    pub total_iterations: u32,
    /// Total prompt tokens summed across every model call.
    pub total_input_tokens: u64,
    /// Total completion tokens summed across every model call.
    pub total_output_tokens: u64,
    /// Wall-clock session duration in milliseconds.
    pub duration_ms: u64,
    /// `true` when the session drained cleanly (every in-flight
    /// child cooperatively cancelled inside the grace window),
    /// `false` when at least one in-flight child timed out under
    /// the configured grace period.
    pub clean_shutdown: bool,
}

/// Stable in-payload discriminator for [`SessionStopRecordPayload`].
pub const RECORD_KIND_SESSION_STOP: &str = "session_stop";

/// Per-session telemetry handed back to [`FleetDaemon::run`] when
/// the shutdown token fires so a `SessionStop` audit row can be
/// written with accurate totals. Surface code populates this
/// struct as turns complete and passes it via
/// [`FleetDaemon::record_session`] before invoking `run`.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// Session id (the same string used in `prepare_session`).
    pub session_id: String,
    /// Root agent id of the session.
    pub agent_id: AgentId,
    /// Iterations consumed so far.
    pub total_iterations: u32,
    /// Input tokens consumed so far.
    pub total_input_tokens: u64,
    /// Output tokens consumed so far.
    pub total_output_tokens: u64,
    /// Wall-clock start (the timestamp the session was first
    /// registered). `duration_ms` in the audit row is computed
    /// from this against `Instant::now()` at shutdown.
    pub started_at: std::time::Instant,
}

pub use aura_fleet_dispatch::AgentJob;
pub use aura_fleet_mailbox::{Mailbox, MailboxConfig, MailboxError, MailboxSender};
pub use aura_plugin_core::PluginLoadError;

/// Inputs to the documented Phase 9 [`AgentMode`] resolution
/// priority. Bundled into a struct so child-agent inheritance can
/// hand the same shape to [`resolve_session_mode`] when narrowing.
///
/// Priority (highest precedence first):
///
/// 1. `cli_flag` — `aura --mode <agent|plan|ask|debug>`
/// 2. `tui_slash` — TUI `/mode <agent|plan|ask|debug>` slash
///    command
/// 3. `sdk_field` — `aura_surface_sdk::SessionConfig::mode`
/// 4. `daemon_default` — `aura_config::FleetConfig::default_mode`
/// 5. Fallback — [`AgentMode::Agent`]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentModeInputs {
    /// CLI flag override (`aura --mode <name>`).
    pub cli_flag: Option<AgentMode>,
    /// TUI slash-command override (`/mode <name>`).
    pub tui_slash: Option<AgentMode>,
    /// SDK [`SessionConfig::mode`] field.
    pub sdk_field: Option<AgentMode>,
    /// Daemon-wide default
    /// ([`FleetConfig::default_mode`]).
    pub daemon_default: Option<AgentMode>,
}

/// Resolve the session [`AgentMode`] from the documented Phase 9
/// priority chain.
///
/// The fallback is [`AgentMode::Agent`] when every input is
/// `None`. See [`AgentModeInputs`] for the per-rung semantics.
#[must_use]
pub fn resolve_session_mode(inputs: AgentModeInputs) -> AgentMode {
    inputs
        .cli_flag
        .or(inputs.tui_slash)
        .or(inputs.sdk_field)
        .or(inputs.daemon_default)
        .unwrap_or(AgentMode::Agent)
}

/// Convenience: extract the daemon-default rung from a typed
/// [`FleetConfig`].
#[must_use]
pub fn daemon_default_mode(fleet: &FleetConfig) -> AgentMode {
    fleet.default_mode
}

/// Wiring config consumed at daemon construction time.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Per-spawn config forwarded to the [`FleetSpawner`].
    pub spawner: FleetSpawnerConfig,
    /// Mailbox capacity / backpressure config.
    pub mailbox: MailboxConfig,
    /// Override the orphan store root. `None` uses
    /// [`OrphanStore::default_root`] (`~/.aura/state/orphans/`).
    pub orphan_root: Option<PathBuf>,
    /// Phase 10 carve-out 4: wall-clock grace period the
    /// shutdown loop waits for in-flight children to settle after
    /// the external [`CancellationToken`] fires. Defaults to
    /// [`DEFAULT_SHUTDOWN_GRACE`] (30 s). Sessions whose children
    /// survive past this window are tagged `clean_shutdown: false`
    /// on their emitted `SessionStop` audit row; surviving
    /// `SpawnMode::Detached` agents are already handed off to the
    /// orphan store by the spawner itself.
    pub shutdown_grace: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            spawner: FleetSpawnerConfig::default(),
            mailbox: MailboxConfig::default(),
            orphan_root: None,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }
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
    /// Sessions currently running on the daemon. Populated via
    /// [`FleetDaemon::record_session`] and drained at shutdown to
    /// emit one [`aura_store_record::RecordKind::SessionStop`]
    /// audit row per session.
    sessions: Mutex<Vec<SessionRecord>>,
    /// Store handle used by [`FleetDaemon::run`] to write the
    /// `SessionStop` audit rows through
    /// [`aura_agent_kernel::write_system_record`]. Same `Arc` the
    /// spawner sees, so the writes go through the kernel's single
    /// record-write path.
    store: Arc<dyn Store>,
    /// Fleet-wide cancellation token cloned into every spawn's
    /// [`FleetSpawnerConfig::fleet_shutdown`] slot when the
    /// external shutdown token fires. Stored on the daemon so the
    /// shutdown path can reach every in-flight child without
    /// needing to enumerate them.
    fleet_shutdown: CancellationToken,
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
        let fleet_shutdown = CancellationToken::new();
        // Phase 10 carve-out 4: derive a spawner config that wires
        // the daemon-owned fleet-wide cancellation token. Any
        // overrides the caller supplied are preserved by
        // `fleet_shutdown.or(existing)`.
        let mut spawner_config = config.spawner.clone();
        if spawner_config.fleet_shutdown.is_none() {
            spawner_config.fleet_shutdown = Some(fleet_shutdown.clone());
        }
        let spawner = Arc::new(FleetSpawner::with_default_derivation(
            store.clone(),
            registry.clone(),
            quota.clone(),
            leases.clone(),
            orphans.clone(),
            child_runner,
            spawner_config,
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
            sessions: Mutex::new(Vec::new()),
            store,
            fleet_shutdown,
        }
    }

    /// Register a session for shutdown bookkeeping. Surface code
    /// calls this when a new top-level session boots; the daemon
    /// emits one [`aura_store_record::RecordKind::SessionStop`]
    /// audit row per registered session when the shutdown token
    /// fires.
    pub async fn record_session(&self, session: SessionRecord) {
        let mut guard = self.sessions.lock().await;
        guard.push(session);
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

    /// Phase 10 carve-out 4: drive the mailbox until the external
    /// [`CancellationToken`] fires OR the mailbox is drained
    /// because every sender has dropped.
    ///
    /// When `shutdown` fires:
    ///
    /// 1. Stop accepting new jobs (the mailbox receiver is dropped
    ///    so subsequent `MailboxSender::send` calls fail with a
    ///    closed-channel error).
    /// 2. Cancel every in-flight child via the daemon-owned
    ///    `fleet_shutdown` token cloned into each spawn's
    ///    [`FleetSpawnerConfig::fleet_shutdown`] slot.
    /// 3. Wait up to [`DaemonConfig::shutdown_grace`] for the
    ///    in-flight children to settle.
    /// 4. Emit one
    ///    [`aura_store_record::RecordKind::SessionStop`] audit row
    ///    per registered session through
    ///    [`aura_agent_kernel::write_system_record`]. Sessions
    ///    whose children survived past the grace window are
    ///    flagged `clean_shutdown: false`.
    /// 5. `SpawnMode::Detached` survivors persist in the
    ///    [`OrphanStore`] (already written by the spawn path) so
    ///    `aura agents reap` can pick them up later.
    ///
    /// Calling `run` a second time after the first run consumed
    /// the receiver returns [`FleetDaemonError::ReceiverAlreadyTaken`].
    ///
    /// # Errors
    ///
    /// See [`FleetDaemonError`].
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), FleetDaemonError> {
        let mut receiver = self
            .receiver
            .lock()
            .await
            .take()
            .ok_or(FleetDaemonError::ReceiverAlreadyTaken)?;
        let dispatcher = self.handle.dispatcher();
        info!("fleet daemon: mailbox loop entered (Phase 10 carve-out 4 shutdown wiring)");
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    info!("fleet daemon: external shutdown observed; draining in-flight children");
                    break;
                }
                maybe_job = receiver.recv() => {
                    let Some(job) = maybe_job else {
                        info!("fleet daemon: mailbox drained — exiting cleanly");
                        self.emit_session_stop_records(true).await;
                        return Ok(());
                    };
                    if let Err(err) = dispatcher.spawn_one(job).await {
                        warn!(error = %err, "fleet daemon: dispatch error");
                    }
                }
            }
        }
        // Phase 10 5/4 shutdown sequence: cancel children, await
        // the grace window, then emit `SessionStop` rows.
        drop(receiver);
        self.fleet_shutdown.cancel();
        let registry = self.handle.registry();
        let grace = self.config.shutdown_grace;
        let drained_cleanly = wait_for_registry_drain(&registry, grace).await;
        self.emit_session_stop_records(drained_cleanly).await;
        info!(
            clean_shutdown = drained_cleanly,
            "fleet daemon: shutdown complete"
        );
        Ok(())
    }

    /// Emit a `RecordKind::SessionStop` audit row through
    /// [`aura_agent_kernel::write_system_record`] for every
    /// session that was registered via [`Self::record_session`].
    ///
    /// `clean_shutdown` carries the Phase 10 carve-out 2 contract:
    /// `true` when every in-flight child cooperatively settled
    /// inside the grace window; `false` otherwise.
    async fn emit_session_stop_records(&self, clean_shutdown: bool) {
        let sessions = {
            let mut guard = self.sessions.lock().await;
            std::mem::take(&mut *guard)
        };
        for session in sessions {
            let duration_ms =
                u64::try_from(session.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let payload = SessionStopRecordPayload {
                kind: RECORD_KIND_SESSION_STOP.to_string(),
                session_id: session.session_id.clone(),
                agent_id: session.agent_id.to_hex(),
                total_iterations: session.total_iterations,
                total_input_tokens: session.total_input_tokens,
                total_output_tokens: session.total_output_tokens,
                duration_ms,
                clean_shutdown,
            };
            let bytes = match serde_json::to_vec(&payload) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "fleet daemon: failed to serialize SessionStop payload");
                    continue;
                }
            };
            let tx = Transaction::new_chained(
                session.agent_id,
                TransactionType::System,
                Bytes::from(bytes),
                None,
            );
            if let Err(e) =
                aura_agent_kernel::write_system_record(&self.store, session.agent_id, tx)
            {
                warn!(
                    session_id = %session.session_id,
                    agent_id = %session.agent_id,
                    error = %e,
                    "fleet daemon: write_system_record(SessionStop) failed"
                );
            }
        }
    }
}

/// Poll the [`FleetRegistry`] until [`FleetRegistry::running_count`]
/// is zero, bounded by `grace`. Returns `true` when the registry
/// drained cleanly inside the window, `false` when the timeout
/// elapsed first.
///
/// Implementation note: we poll at 50 ms intervals rather than
/// subscribe to a registry signal because the registry's shape is
/// intentionally lock-light. A subscription-based API is tracked
/// as a Phase 11 follow-up.
async fn wait_for_registry_drain(registry: &FleetRegistry, grace: Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if registry.running_count() == 0 {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
