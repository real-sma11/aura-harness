//! Best-effort trigger-metadata registration to the swarm gateway
//! (Swarm TEE upgrade phase 8, "trigger outside, data inside").
//!
//! After any process mutation (create / update / delete, or a trigger
//! firing and advancing `next_run_at`), the harness pushes its current
//! [`aura_store_db::ProcessStore::trigger_metadata`] set to the swarm
//! gateway's internal replace-sync endpoint
//! (`PUT /internal/agents/{agent_id}/process-triggers`) so the
//! external `ProcessCronService` knows *when* to fire each process.
//!
//! # Trust boundary
//!
//! The payload is `Vec<ProcessTriggerMeta>` — structurally just
//! `(process_id, cron, enabled, next_run_at)`. Prompts, config, and
//! run history never leave the VM; this module must never serialize
//! anything richer than [`ProcessTriggerMeta`].
//!
//! # Operational behavior
//!
//! * **Local / dev agents:** when the swarm env isn't configured the
//!   registrar is a silent no-op (debug log only). Local agents will
//!   register through aura-storage scheduling rows in a later phase.
//! * **Best-effort:** pushes run on a background task with a small
//!   bounded retry. A failed push logs a warning and is retried on
//!   the next mutation — it never fails or delays the user's API call.
//!
//! # Configuration (mirrors the swarm gateway's internal auth)
//!
//! * `AURA_SWARM_INTERNAL_URL` — gateway base URL (falls back to the
//!   `CONTROL_PLANE_URL` the swarm scheduler already injects into
//!   agent pods).
//! * `AURA_SWARM_INTERNAL_TOKEN` — bearer token matching the
//!   gateway's `INTERNAL_TOKEN` for `/internal` routes.
//! * `AGENT_ID` (fallback `AURA_MACHINE_ID` / `MACHINE_ID`) — this
//!   agent's id, as injected by the swarm pod builder.

use std::sync::Arc;
use std::time::Duration;

use aura_store_db::ProcessStore;

/// Bounded retry schedule for one push attempt cycle.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(200), Duration::from_secs(1)];

/// Per-request timeout for the registration PUT.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Where (and as whom) to register trigger metadata.
#[derive(Clone)]
pub struct RegistrarTarget {
    /// Swarm gateway base URL, e.g. `http://aura-swarm-gateway:8080`.
    pub base_url: String,
    /// Internal bearer token (the gateway's `INTERNAL_TOKEN`).
    pub token: String,
    /// This agent's id (hex), as assigned by the swarm control plane.
    pub agent_id: String,
}

impl std::fmt::Debug for RegistrarTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrarTarget")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

/// Outcome of one synchronous push cycle (exposed for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// No swarm env configured — local/dev agent, nothing to do.
    Skipped,
    /// The gateway accepted the full desired set.
    Pushed,
    /// All attempts failed; will be retried on the next mutation.
    Failed,
}

/// Best-effort pusher of the exportable trigger-metadata set.
pub struct TriggerRegistrar {
    target: Option<RegistrarTarget>,
    store: Arc<ProcessStore>,
    client: reqwest::Client,
}

impl TriggerRegistrar {
    /// Build a registrar with an explicit target (`None` = no-op mode).
    pub fn new(target: Option<RegistrarTarget>, store: Arc<ProcessStore>) -> Self {
        Self {
            target,
            store,
            client: reqwest::Client::new(),
        }
    }

    /// Build a registrar from the environment. Returns a no-op
    /// registrar (debug-logged) unless the swarm gateway URL, internal
    /// token, and agent id are all present.
    pub fn from_env(store: Arc<ProcessStore>) -> Self {
        let base_url = env_non_empty("AURA_SWARM_INTERNAL_URL")
            .or_else(|| env_non_empty("CONTROL_PLANE_URL"));
        let token = env_non_empty("AURA_SWARM_INTERNAL_TOKEN");
        let agent_id = env_non_empty("AGENT_ID")
            .or_else(|| env_non_empty("AURA_MACHINE_ID"))
            .or_else(|| env_non_empty("MACHINE_ID"));

        let target = match (base_url, token, agent_id) {
            (Some(base_url), Some(token), Some(agent_id)) => {
                tracing::info!(
                    %base_url,
                    %agent_id,
                    "trigger registrar active: process-trigger metadata will sync to the swarm gateway"
                );
                Some(RegistrarTarget {
                    base_url,
                    token,
                    agent_id,
                })
            }
            _ => {
                tracing::debug!(
                    "trigger registrar disabled: AURA_SWARM_INTERNAL_URL / \
                     AURA_SWARM_INTERNAL_TOKEN / AGENT_ID not all set (local agent)"
                );
                None
            }
        };

        Self::new(target, store)
    }

    /// Whether a swarm target is configured (i.e. pushes are live).
    pub fn is_active(&self) -> bool {
        self.target.is_some()
    }

    /// Fire-and-forget sync: pushes the current trigger-metadata set
    /// on a background task. Never blocks or fails the caller.
    pub fn sync(self: &Arc<Self>) {
        if self.target.is_none() {
            tracing::debug!("trigger registrar: no swarm target configured; skipping sync");
            return;
        }
        let registrar = Arc::clone(self);
        tokio::spawn(async move {
            registrar.sync_now().await;
        });
    }

    /// Push the current trigger-metadata set, awaiting completion.
    /// Used directly by tests; production goes through [`Self::sync`].
    pub async fn sync_now(&self) -> SyncOutcome {
        let Some(target) = &self.target else {
            return SyncOutcome::Skipped;
        };

        // The payload is the exportable metadata view and nothing
        // else: ProcessTriggerMeta structurally cannot carry the
        // prompt, config, or run history (trust boundary).
        let metas = match self.store.trigger_metadata() {
            Ok(metas) => metas,
            Err(e) => {
                tracing::warn!(error = %e, "trigger registrar: failed to read trigger metadata");
                return SyncOutcome::Failed;
            }
        };

        let url = format!(
            "{}/internal/agents/{}/process-triggers",
            target.base_url.trim_end_matches('/'),
            target.agent_id
        );

        let mut attempt = 0usize;
        loop {
            let result = self
                .client
                .put(&url)
                .bearer_auth(&target.token)
                .json(&metas)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    tracing::debug!(count = metas.len(), "registered process triggers with swarm gateway");
                    return SyncOutcome::Pushed;
                }
                Ok(resp) => {
                    tracing::warn!(
                        status = %resp.status(),
                        attempt,
                        "swarm gateway rejected trigger registration"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "trigger registration push failed");
                }
            }

            let Some(delay) = RETRY_DELAYS.get(attempt) else {
                tracing::warn!(
                    count = metas.len(),
                    "giving up on trigger registration; will retry on next process mutation"
                );
                return SyncOutcome::Failed;
            };
            tokio::time::sleep(*delay).await;
            attempt += 1;
        }
    }
}

impl std::fmt::Debug for TriggerRegistrar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TriggerRegistrar")
            .field("target", &self.target)
            .finish()
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_store_db::{NewProcess, RocksStore};

    fn test_process_store() -> Arc<ProcessStore> {
        let dir = tempfile::tempdir().unwrap().keep();
        let rocks = RocksStore::open(&dir, false).unwrap();
        Arc::new(ProcessStore::new(rocks.db_handle().clone()))
    }

    fn seed_process(store: &ProcessStore) {
        store
            .create(NewProcess {
                name: "nightly report".into(),
                description: Some("summarize the day".into()),
                cron: "0 3 * * *".into(),
                prompt: "IN-TEE-PROMPT: read my sealed notes and write a report".into(),
                config: Some(
                    serde_json::json!({"model": "secret-model-choice"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                enabled: true,
            })
            .unwrap();
    }

    /// The registration payload may carry exactly the four metadata
    /// fields — never the prompt, config, or anything else from the
    /// sealed process record.
    #[test]
    fn payload_contains_only_trigger_metadata_fields() {
        let store = test_process_store();
        seed_process(&store);

        let metas = store.trigger_metadata().unwrap();
        let json = serde_json::to_value(&metas).unwrap();
        let entries = json.as_array().unwrap();
        assert_eq!(entries.len(), 1);

        let allowed = ["process_id", "cron", "enabled", "next_run_at"];
        for entry in entries {
            for key in entry.as_object().unwrap().keys() {
                assert!(
                    allowed.contains(&key.as_str()),
                    "field `{key}` must not cross the trust boundary"
                );
            }
        }

        let raw = serde_json::to_string(&metas).unwrap();
        assert!(!raw.contains("IN-TEE-PROMPT"));
        assert!(!raw.contains("secret-model-choice"));
        assert!(!raw.contains("nightly report"));
    }

    /// Without swarm env config the registrar is a silent no-op: no
    /// HTTP, no error — just `Skipped`.
    #[tokio::test]
    async fn sync_is_noop_without_config() {
        let store = test_process_store();
        seed_process(&store);

        let registrar = TriggerRegistrar::new(None, store);
        assert!(!registrar.is_active());
        assert_eq!(registrar.sync_now().await, SyncOutcome::Skipped);
    }
}
