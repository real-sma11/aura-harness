//! First-class processes / automations (Swarm TEE upgrade phase 7).
//!
//! A *process* is a cron-scheduled instruction the agent executes
//! autonomously: `name + cron + prompt (+ optional JSON config)`.
//! Definitions live in the `processes` column family and execution
//! history in the `process_runs` column family of the shared
//! agent-state RocksDB, both behind the same per-value sealing
//! envelope as the secrets vault (see [`crate::seal`] and
//! [`crate::vault`]):
//!
//! * **Sealed mode** (`AURA_STATE_ENCRYPTION=sealed`): process records
//!   (including the prompt and config) and run records are AES-256-GCM
//!   encrypted under the per-agent DEK — never written to disk in
//!   plaintext.
//! * **Plaintext/dev mode**: plain JSON, like the rest of the state.
//!
//! # Trust boundary ("trigger outside, data inside")
//!
//! The process prompt, config, and run history are **in-TEE data** and
//! must never leave the agent VM. The only process-derived data
//! allowed off-VM is the trigger metadata returned by
//! [`ProcessStore::trigger_metadata`]: `(process_id, cron, enabled,
//! next_run_at)` — enough for an external cron service to know *when*
//! to fire a trigger, and nothing about *what* the trigger does.
//!
//! [`ProcessRecord`] has a manual `Debug` impl that redacts the prompt
//! and config so neither can leak through `{:?}` formatting in logs.

use crate::seal::SealCipher;
use chrono::{DateTime, Utc};
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;

/// Maximum length of a process name.
pub const MAX_PROCESS_NAME_LEN: usize = 200;

/// Maximum size of a process prompt in bytes.
pub const MAX_PROCESS_PROMPT_BYTES: usize = 64 * 1024;

/// Run records kept per process; older runs are pruned on insert.
pub const MAX_PROCESS_RUNS_KEPT: usize = 50;

/// Errors produced by [`ProcessStore`].
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The process name is empty or too long.
    #[error("invalid process name: {0}")]
    InvalidName(String),
    /// The cron expression failed to parse.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    /// The prompt is empty or exceeds [`MAX_PROCESS_PROMPT_BYTES`].
    #[error("invalid process prompt: {0}")]
    InvalidPrompt(String),
    /// No process with the given id exists.
    #[error("process not found: {0}")]
    NotFound(String),
    /// No run with the given id exists for the process.
    #[error("process run not found: {0}")]
    RunNotFound(String),
    /// Underlying RocksDB / sealing failure.
    #[error("process store error: {0}")]
    Store(String),
    /// JSON (de)serialization failure.
    #[error("process serialization error: {0}")]
    Serde(String),
}

/// A persisted process definition.
///
/// Deliberately **no derived `Debug`** — the manual impl below redacts
/// the prompt and config so the in-TEE payload cannot leak through
/// log/tracing formatting.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// Unique process id (UUID v4 string; the store key).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Cron expression (UTC). Standard 5-field expressions are
    /// accepted and normalized; 6/7-field (with seconds / year) work
    /// as-is.
    pub cron: String,
    /// The instruction the agent executes on each run. In-TEE data —
    /// never exported off-VM.
    pub prompt: String,
    /// Optional structured config the run may consult. In-TEE data —
    /// never exported off-VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    /// Whether the scheduler should fire this process.
    pub enabled: bool,
    /// Creation timestamp (preserved across updates).
    pub created_at: DateTime<Utc>,
    /// Last-update timestamp.
    pub updated_at: DateTime<Utc>,
    /// When the process last ran (trigger time), if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    /// Next scheduled fire time computed from [`Self::cron`] (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for ProcessRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRecord")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("cron", &self.cron)
            .field("prompt", &"<redacted>")
            .field("config", &self.config.as_ref().map(|_| "<redacted>"))
            .field("enabled", &self.enabled)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("last_run_at", &self.last_run_at)
            .field("next_run_at", &self.next_run_at)
            .finish()
    }
}

/// Terminal / in-flight status of one process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRunStatus {
    /// The run is currently executing.
    Running,
    /// The run completed successfully.
    Success,
    /// The run failed.
    Failure,
}

/// One execution of a process. Sealed at rest like the definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRunRecord {
    /// Unique run id (UUID v4 string).
    pub run_id: String,
    /// Owning process id.
    pub process_id: String,
    /// Internal monotonic ordering key (store-assigned). Wall-clock
    /// millis can tie when runs start within the same millisecond, so
    /// the store key orders on this strictly-increasing sequence
    /// instead.
    #[serde(default)]
    pub seq: u64,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished; `None` while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Current status.
    pub status: ProcessRunStatus,
    /// Short result summary on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Error detail on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Input for [`ProcessStore::create`].
#[derive(Clone, Deserialize)]
pub struct NewProcess {
    /// Human-readable name (required, bounded).
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
    /// Cron expression (validated).
    pub cron: String,
    /// Instruction the agent executes (required, bounded).
    pub prompt: String,
    /// Optional structured config.
    #[serde(default)]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    /// Defaults to `true` when omitted.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

/// Partial update for [`ProcessStore::update`]. `None` fields keep the
/// stored value; `description` / `config` use a nested `Option` so an
/// explicit `null` clears them.
#[derive(Clone, Default, Deserialize)]
pub struct ProcessUpdate {
    /// New name, when present.
    #[serde(default)]
    pub name: Option<String>,
    /// New description; `Some(None)` clears it.
    #[serde(default, with = "double_option")]
    pub description: Option<Option<String>>,
    /// New cron expression, when present (re-validated).
    #[serde(default)]
    pub cron: Option<String>,
    /// New prompt, when present.
    #[serde(default)]
    pub prompt: Option<String>,
    /// New config; `Some(None)` clears it.
    #[serde(default, with = "double_option")]
    pub config: Option<Option<serde_json::Map<String, serde_json::Value>>>,
    /// New enabled flag, when present.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Serde helper distinguishing "field absent" (`None`) from "field
/// explicitly null" (`Some(None)`) on [`ProcessUpdate`].
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(de).map(Some)
    }
}

/// The **only** process-derived data permitted to leave the agent VM
/// ("trigger outside, data inside").
///
/// Structurally contains no prompt, no config, and no run data — just
/// what an external cron service needs to fire a content-free trigger:
/// which process, on what schedule, whether it is active, and the next
/// computed fire time. The next phase exports this to the swarm
/// gateway's `process_triggers` CF; nothing else from this module may
/// cross the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTriggerMeta {
    /// Process id (opaque off-VM).
    pub process_id: String,
    /// Cron expression (UTC) — schedule only, no payload.
    pub cron: String,
    /// Whether the trigger should fire.
    pub enabled: bool,
    /// Next computed fire time, if the schedule has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
}

/// Normalize a cron expression for the `cron` crate, which expects a
/// seconds field: standard 5-field expressions get `0 ` prepended so
/// `*/5 * * * *` means "every 5 minutes at second 0". 6/7-field
/// expressions pass through unchanged.
fn normalize_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_string()
    }
}

/// Validate a cron expression (5, 6, or 7 fields; UTC semantics).
///
/// # Errors
/// Returns [`ProcessError::InvalidCron`] when the expression does not
/// parse.
pub fn validate_cron(expr: &str) -> Result<cron::Schedule, ProcessError> {
    if expr.trim().is_empty() {
        return Err(ProcessError::InvalidCron(
            "cron expression must not be empty".into(),
        ));
    }
    cron::Schedule::from_str(&normalize_cron(expr))
        .map_err(|e| ProcessError::InvalidCron(e.to_string()))
}

/// Compute the next fire time (UTC) strictly after `after`.
///
/// # Errors
/// Returns [`ProcessError::InvalidCron`] when the expression does not
/// parse.
pub fn next_run_after(
    expr: &str,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, ProcessError> {
    let schedule = validate_cron(expr)?;
    Ok(schedule.after(&after).next())
}

fn validate_name(name: &str) -> Result<(), ProcessError> {
    if name.trim().is_empty() {
        return Err(ProcessError::InvalidName("name must not be empty".into()));
    }
    if name.len() > MAX_PROCESS_NAME_LEN {
        return Err(ProcessError::InvalidName(format!(
            "name exceeds {MAX_PROCESS_NAME_LEN} characters"
        )));
    }
    Ok(())
}

fn validate_prompt(prompt: &str) -> Result<(), ProcessError> {
    if prompt.trim().is_empty() {
        return Err(ProcessError::InvalidPrompt(
            "prompt must not be empty".into(),
        ));
    }
    if prompt.len() > MAX_PROCESS_PROMPT_BYTES {
        return Err(ProcessError::InvalidPrompt(format!(
            "prompt exceeds {MAX_PROCESS_PROMPT_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Key for a run record: `{process_id}/{seq:020}/{run_id}`.
///
/// RocksDB's lexicographic key order then sorts a process's runs
/// oldest-first under the `{process_id}/` prefix, which makes both
/// "newest N" listing and oldest-first pruning a single prefix scan.
/// `seq` is the store-assigned strictly-increasing sequence (see
/// [`next_run_seq`]) so two runs started in the same millisecond still
/// order deterministically by start order.
fn run_key(process_id: &str, seq: u64, run_id: &str) -> Vec<u8> {
    format!("{process_id}/{seq:020}/{run_id}").into_bytes()
}

/// Strictly-increasing run sequence, seeded from wall-clock millis so
/// it stays roughly time-ordered across process restarts.
fn next_run_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    #[allow(clippy::cast_sign_loss)]
    let now = Utc::now().timestamp_millis().max(0) as u64;
    let mut prev = LAST.load(Ordering::SeqCst);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange(prev, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn run_prefix(process_id: &str) -> Vec<u8> {
    format!("{process_id}/").into_bytes()
}

/// Sealed store for process definitions and run history, backed by the
/// shared agent-state RocksDB. Same construction pattern as
/// [`crate::vault::SecretsVault`]: shared DB handle plus the optional
/// value cipher decided at boot.
pub struct ProcessStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
    /// `Some` in sealed mode (records are AES-256-GCM ciphertext at
    /// rest); `None` is plaintext/dev mode.
    cipher: Option<Arc<SealCipher>>,
}

impl ProcessStore {
    /// Create a plaintext-mode store on the given shared DB handle.
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db, cipher: None }
    }

    /// Create a store with optional sealed (encrypted-at-rest) values.
    #[must_use]
    pub const fn with_cipher(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        cipher: Option<Arc<SealCipher>>,
    ) -> Self {
        Self { db, cipher }
    }

    fn seal_value(&self, plain: Vec<u8>) -> Result<Vec<u8>, ProcessError> {
        match &self.cipher {
            Some(cipher) => cipher
                .seal(&plain)
                .map_err(|e| ProcessError::Store(format!("sealing value: {e}"))),
            None => Ok(plain),
        }
    }

    fn open_value<'a>(&self, bytes: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, ProcessError> {
        match &self.cipher {
            Some(cipher) => cipher
                .open(bytes)
                .map(std::borrow::Cow::Owned)
                .map_err(|e| ProcessError::Store(format!("opening sealed value: {e}"))),
            None => Ok(std::borrow::Cow::Borrowed(bytes)),
        }
    }

    fn processes_cf(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, ProcessError> {
        self.db
            .cf_handle(crate::cf::PROCESSES)
            .ok_or_else(|| ProcessError::Store("processes column family not found".into()))
    }

    fn runs_cf(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, ProcessError> {
        self.db
            .cf_handle(crate::cf::PROCESS_RUNS)
            .ok_or_else(|| ProcessError::Store("process_runs column family not found".into()))
    }

    fn put_record(&self, record: &ProcessRecord) -> Result<(), ProcessError> {
        let plain = serde_json::to_vec(record).map_err(|e| ProcessError::Serde(e.to_string()))?;
        let sealed = self.seal_value(plain)?;
        let cf = self.processes_cf()?;
        self.db
            .put_cf(&cf, record.id.as_bytes(), sealed)
            .map_err(|e| ProcessError::Store(e.to_string()))
    }

    /// Create a new process. Validates name, prompt, and cron; computes
    /// the initial `next_run_at` from the cron expression (UTC).
    ///
    /// # Errors
    /// Rejects invalid input; surfaces store / sealing failures.
    pub fn create(&self, input: NewProcess) -> Result<ProcessRecord, ProcessError> {
        validate_name(&input.name)?;
        validate_prompt(&input.prompt)?;
        let now = Utc::now();
        let next_run_at = next_run_after(&input.cron, now)?;

        let record = ProcessRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: input.name,
            description: input.description,
            cron: input.cron,
            prompt: input.prompt,
            config: input.config,
            enabled: input.enabled,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            next_run_at,
        };
        self.put_record(&record)?;
        Ok(record)
    }

    /// Fetch a process by id.
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn get(&self, id: &str) -> Result<Option<ProcessRecord>, ProcessError> {
        let cf = self.processes_cf()?;
        match self
            .db
            .get_cf(&cf, id.as_bytes())
            .map_err(|e| ProcessError::Store(e.to_string()))?
        {
            Some(bytes) => {
                let bytes = self.open_value(&bytes)?;
                let record: ProcessRecord = serde_json::from_slice(&bytes)
                    .map_err(|e| ProcessError::Serde(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// List all processes (full definitions — in-VM consumers only).
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn list(&self) -> Result<Vec<ProcessRecord>, ProcessError> {
        let cf = self.processes_cf()?;
        let mut out = Vec::new();
        for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
            let (_, v) = item.map_err(|e| ProcessError::Store(e.to_string()))?;
            let bytes = self.open_value(&v)?;
            let record: ProcessRecord =
                serde_json::from_slice(&bytes).map_err(|e| ProcessError::Serde(e.to_string()))?;
            out.push(record);
        }
        Ok(out)
    }

    /// Apply a partial update. `created_at` and run bookkeeping are
    /// preserved; `updated_at` is refreshed and `next_run_at` is
    /// recomputed from the (possibly new) cron expression.
    ///
    /// # Errors
    /// [`ProcessError::NotFound`] when the id is unknown; rejects
    /// invalid replacement values.
    pub fn update(&self, id: &str, update: ProcessUpdate) -> Result<ProcessRecord, ProcessError> {
        let mut record = self
            .get(id)?
            .ok_or_else(|| ProcessError::NotFound(id.to_string()))?;

        if let Some(name) = update.name {
            validate_name(&name)?;
            record.name = name;
        }
        if let Some(description) = update.description {
            record.description = description;
        }
        if let Some(cron_expr) = update.cron {
            validate_cron(&cron_expr)?;
            record.cron = cron_expr;
        }
        if let Some(prompt) = update.prompt {
            validate_prompt(&prompt)?;
            record.prompt = prompt;
        }
        if let Some(config) = update.config {
            record.config = config;
        }
        if let Some(enabled) = update.enabled {
            record.enabled = enabled;
        }

        let now = Utc::now();
        record.updated_at = now;
        record.next_run_at = next_run_after(&record.cron, now)?;
        self.put_record(&record)?;
        Ok(record)
    }

    /// Delete a process and its entire run history. Returns `true`
    /// when a record existed.
    ///
    /// # Errors
    /// Surfaces store failures.
    pub fn delete(&self, id: &str) -> Result<bool, ProcessError> {
        let existed = self.get(id)?.is_some();
        if existed {
            let cf = self.processes_cf()?;
            self.db
                .delete_cf(&cf, id.as_bytes())
                .map_err(|e| ProcessError::Store(e.to_string()))?;
            // Drop run history.
            let runs_cf = self.runs_cf()?;
            for key in self.run_keys(id)? {
                self.db
                    .delete_cf(&runs_cf, key)
                    .map_err(|e| ProcessError::Store(e.to_string()))?;
            }
        }
        Ok(existed)
    }

    /// Record the start of a run: creates a `Running` run record,
    /// stamps `last_run_at = now`, recomputes `next_run_at`, and prunes
    /// history beyond [`MAX_PROCESS_RUNS_KEPT`].
    ///
    /// # Errors
    /// [`ProcessError::NotFound`] when the process id is unknown.
    pub fn start_run(&self, process_id: &str) -> Result<ProcessRunRecord, ProcessError> {
        let mut record = self
            .get(process_id)?
            .ok_or_else(|| ProcessError::NotFound(process_id.to_string()))?;

        let now = Utc::now();
        let run = ProcessRunRecord {
            run_id: uuid::Uuid::new_v4().to_string(),
            process_id: process_id.to_string(),
            seq: next_run_seq(),
            started_at: now,
            finished_at: None,
            status: ProcessRunStatus::Running,
            summary: None,
            error: None,
        };
        self.put_run(&run)?;

        record.last_run_at = Some(now);
        record.updated_at = now;
        record.next_run_at = next_run_after(&record.cron, now)?;
        self.put_record(&record)?;

        self.prune_runs(process_id)?;
        Ok(run)
    }

    /// Mark a run finished with the given terminal status.
    ///
    /// # Errors
    /// [`ProcessError::RunNotFound`] when no matching run exists (it
    /// may have been pruned).
    pub fn finish_run(
        &self,
        process_id: &str,
        run_id: &str,
        status: ProcessRunStatus,
        summary: Option<String>,
        error: Option<String>,
    ) -> Result<ProcessRunRecord, ProcessError> {
        let mut run = self
            .list_runs(process_id)?
            .into_iter()
            .find(|r| r.run_id == run_id)
            .ok_or_else(|| ProcessError::RunNotFound(run_id.to_string()))?;
        run.finished_at = Some(Utc::now());
        run.status = status;
        run.summary = summary;
        run.error = error;
        self.put_run(&run)?;
        Ok(run)
    }

    /// Run history for a process, newest first. Bounded by
    /// [`MAX_PROCESS_RUNS_KEPT`] via insert-time pruning.
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn list_runs(&self, process_id: &str) -> Result<Vec<ProcessRunRecord>, ProcessError> {
        let cf = self.runs_cf()?;
        let prefix = run_prefix(process_id);
        let mut out = Vec::new();
        for item in self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        ) {
            let (k, v) = item.map_err(|e| ProcessError::Store(e.to_string()))?;
            if !k.starts_with(&prefix) {
                break;
            }
            let bytes = self.open_value(&v)?;
            let run: ProcessRunRecord =
                serde_json::from_slice(&bytes).map_err(|e| ProcessError::Serde(e.to_string()))?;
            out.push(run);
        }
        out.reverse(); // key order is oldest-first
        Ok(out)
    }

    /// Exportable trigger metadata for every process.
    ///
    /// **This is the only process data allowed off-VM.** It carries
    /// `(process_id, cron, enabled, next_run_at)` and structurally
    /// cannot carry the prompt, config, or run history — see
    /// [`ProcessTriggerMeta`]. The next phase registers this with the
    /// swarm gateway's `process_triggers` CF so the external
    /// `ProcessCronService` can fire content-free triggers.
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn trigger_metadata(&self) -> Result<Vec<ProcessTriggerMeta>, ProcessError> {
        Ok(self
            .list()?
            .into_iter()
            .map(|p| ProcessTriggerMeta {
                process_id: p.id,
                cron: p.cron,
                enabled: p.enabled,
                next_run_at: p.next_run_at,
            })
            .collect())
    }

    fn put_run(&self, run: &ProcessRunRecord) -> Result<(), ProcessError> {
        let plain = serde_json::to_vec(run).map_err(|e| ProcessError::Serde(e.to_string()))?;
        let sealed = self.seal_value(plain)?;
        let cf = self.runs_cf()?;
        self.db
            .put_cf(&cf, run_key(&run.process_id, run.seq, &run.run_id), sealed)
            .map_err(|e| ProcessError::Store(e.to_string()))
    }

    /// All run keys for a process, oldest first.
    fn run_keys(&self, process_id: &str) -> Result<Vec<Vec<u8>>, ProcessError> {
        let cf = self.runs_cf()?;
        let prefix = run_prefix(process_id);
        let mut keys = Vec::new();
        for item in self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        ) {
            let (k, _) = item.map_err(|e| ProcessError::Store(e.to_string()))?;
            if !k.starts_with(&prefix) {
                break;
            }
            keys.push(k.to_vec());
        }
        Ok(keys)
    }

    /// Delete oldest runs beyond [`MAX_PROCESS_RUNS_KEPT`].
    fn prune_runs(&self, process_id: &str) -> Result<(), ProcessError> {
        let keys = self.run_keys(process_id)?;
        if keys.len() <= MAX_PROCESS_RUNS_KEPT {
            return Ok(());
        }
        let cf = self.runs_cf()?;
        let excess = keys.len() - MAX_PROCESS_RUNS_KEPT;
        for key in keys.into_iter().take(excess) {
            self.db
                .delete_cf(&cf, key)
                .map_err(|e| ProcessError::Store(e.to_string()))?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ProcessStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose contents or key material.
        f.debug_struct("ProcessStore")
            .field("sealed", &self.cipher.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::{ColumnFamilyDescriptor, Options};

    fn test_db(dir: &std::path::Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![
            ColumnFamilyDescriptor::new(crate::cf::PROCESSES, Options::default()),
            ColumnFamilyDescriptor::new(crate::cf::PROCESS_RUNS, Options::default()),
        ];
        Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
    }

    fn new_process(name: &str) -> NewProcess {
        NewProcess {
            name: name.into(),
            description: Some("nightly report".into()),
            cron: "0 3 * * *".into(),
            prompt: "Summarize yesterday's commits and email the team.".into(),
            config: None,
            enabled: true,
        }
    }

    #[test]
    fn crud_roundtrip_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));

        let created = store.create(new_process("nightly")).unwrap();
        assert!(created.enabled);
        assert!(created.next_run_at.unwrap() > Utc::now());
        assert!(created.last_run_at.is_none());

        let fetched = store.get(&created.id).unwrap().unwrap();
        assert_eq!(fetched.name, "nightly");
        assert_eq!(fetched.prompt, created.prompt);
        assert_eq!(fetched.created_at, created.created_at);

        // Update: rename + disable; created_at preserved.
        let updated = store
            .update(
                &created.id,
                ProcessUpdate {
                    name: Some("nightly-v2".into()),
                    enabled: Some(false),
                    ..ProcessUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(updated.name, "nightly-v2");
        assert!(!updated.enabled);
        assert_eq!(updated.created_at, created.created_at);
        assert!(updated.updated_at >= created.updated_at);
        // Prompt untouched by a partial update.
        assert_eq!(updated.prompt, created.prompt);

        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.delete(&created.id).unwrap());
        assert!(store.get(&created.id).unwrap().is_none());
        assert!(!store.delete(&created.id).unwrap());
    }

    #[test]
    fn invalid_inputs_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));

        let mut bad_cron = new_process("p");
        bad_cron.cron = "not a cron".into();
        assert!(matches!(
            store.create(bad_cron),
            Err(ProcessError::InvalidCron(_))
        ));

        let mut empty_name = new_process("");
        empty_name.name = "  ".into();
        assert!(matches!(
            store.create(empty_name),
            Err(ProcessError::InvalidName(_))
        ));

        let mut empty_prompt = new_process("p");
        empty_prompt.prompt = String::new();
        assert!(matches!(
            store.create(empty_prompt),
            Err(ProcessError::InvalidPrompt(_))
        ));

        let mut huge_prompt = new_process("p");
        huge_prompt.prompt = "x".repeat(MAX_PROCESS_PROMPT_BYTES + 1);
        assert!(matches!(
            store.create(huge_prompt),
            Err(ProcessError::InvalidPrompt(_))
        ));

        // Update with a bad cron is rejected and leaves the record alone.
        let created = store.create(new_process("ok")).unwrap();
        assert!(matches!(
            store.update(
                &created.id,
                ProcessUpdate {
                    cron: Some("garbage".into()),
                    ..ProcessUpdate::default()
                }
            ),
            Err(ProcessError::InvalidCron(_))
        ));
        assert_eq!(store.get(&created.id).unwrap().unwrap().cron, "0 3 * * *");
    }

    #[test]
    fn cron_five_and_six_field_accepted() {
        // Standard 5-field (normalized with a 0-seconds prefix).
        assert!(validate_cron("*/5 * * * *").is_ok());
        // 6-field with seconds.
        assert!(validate_cron("30 */10 * * * *").is_ok());
        // 7-field with year.
        assert!(validate_cron("0 0 4 * * * 2030").is_ok());
        assert!(validate_cron("").is_err());
        assert!(validate_cron("61 * * * *").is_err());

        let next = next_run_after("0 3 * * *", Utc::now()).unwrap().unwrap();
        assert!(next > Utc::now());
        assert_eq!(next.format("%H:%M:%S").to_string(), "03:00:00");
    }

    /// Sealed mode: prompt bytes must not appear in plaintext anywhere
    /// in the raw on-disk column family (same pattern as the vault's
    /// sealed-at-rest test).
    #[test]
    fn sealed_at_rest_prompt_not_plaintext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let cipher = Arc::new(SealCipher::new(&[7u8; 32]));
        let store = ProcessStore::with_cipher(Arc::clone(&db), Some(cipher));

        let prompt = b"check the production dashboards and rotate keys";
        let mut input = new_process("sealed");
        input.prompt = String::from_utf8(prompt.to_vec()).unwrap();
        let created = store.create(input).unwrap();

        // Roundtrip still works through the store.
        assert_eq!(
            store.get(&created.id).unwrap().unwrap().prompt.as_bytes(),
            prompt
        );

        let cf = db.cf_handle(crate::cf::PROCESSES).unwrap();
        let raw = db
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert!(SealCipher::is_sealed(&raw));
        assert!(
            !raw.windows(prompt.len()).any(|w| w == prompt.as_slice()),
            "sealed bytes must not contain the plaintext prompt"
        );

        // Run records are sealed too.
        let run = store.start_run(&created.id).unwrap();
        store
            .finish_run(
                &created.id,
                &run.run_id,
                ProcessRunStatus::Failure,
                None,
                Some("secret-failure-detail".into()),
            )
            .unwrap();
        let runs_cf = db.cf_handle(crate::cf::PROCESS_RUNS).unwrap();
        let raw_run = db
            .iterator_cf(&runs_cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert!(SealCipher::is_sealed(&raw_run));
        let needle = b"secret-failure-detail";
        assert!(!raw_run
            .windows(needle.len())
            .any(|w| w == needle.as_slice()));
    }

    #[test]
    fn run_lifecycle_and_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));
        let created = store.create(new_process("runs")).unwrap();

        let run = store.start_run(&created.id).unwrap();
        assert_eq!(run.status, ProcessRunStatus::Running);
        assert!(run.finished_at.is_none());

        // start_run stamps last_run_at and keeps next_run_at fresh.
        let after = store.get(&created.id).unwrap().unwrap();
        assert!(after.last_run_at.is_some());
        assert!(after.next_run_at.unwrap() > Utc::now());

        let finished = store
            .finish_run(
                &created.id,
                &run.run_id,
                ProcessRunStatus::Success,
                Some("done".into()),
                None,
            )
            .unwrap();
        assert_eq!(finished.status, ProcessRunStatus::Success);
        assert!(finished.finished_at.is_some());

        let runs = store.list_runs(&created.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].summary.as_deref(), Some("done"));

        // Unknown run id errors.
        assert!(matches!(
            store.finish_run(&created.id, "nope", ProcessRunStatus::Failure, None, None),
            Err(ProcessError::RunNotFound(_))
        ));
    }

    #[test]
    fn run_history_capped_at_max() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));
        let created = store.create(new_process("capped")).unwrap();

        let total = MAX_PROCESS_RUNS_KEPT + 7;
        let mut run_ids = Vec::new();
        for _ in 0..total {
            run_ids.push(store.start_run(&created.id).unwrap().run_id);
        }

        let runs = store.list_runs(&created.id).unwrap();
        assert_eq!(runs.len(), MAX_PROCESS_RUNS_KEPT, "history is capped");
        // Newest-first listing: the most recent run id comes first and
        // the oldest 7 were pruned.
        assert_eq!(runs[0].run_id, *run_ids.last().unwrap());
        let kept: std::collections::HashSet<_> = runs.iter().map(|r| r.run_id.clone()).collect();
        for pruned in &run_ids[..7] {
            assert!(!kept.contains(pruned), "oldest runs must be pruned");
        }
    }

    #[test]
    fn delete_removes_run_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));
        let created = store.create(new_process("cleanup")).unwrap();
        store.start_run(&created.id).unwrap();
        store.start_run(&created.id).unwrap();
        assert_eq!(store.list_runs(&created.id).unwrap().len(), 2);

        assert!(store.delete(&created.id).unwrap());
        assert!(store.list_runs(&created.id).unwrap().is_empty());
    }

    #[test]
    fn trigger_metadata_contains_no_sensitive_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(test_db(dir.path()));
        let mut input = new_process("exportable");
        input.prompt = "ULTRA-SENSITIVE-INSTRUCTION".into();
        let mut config = serde_json::Map::new();
        config.insert(
            "target".into(),
            serde_json::Value::String("CONFIDENTIAL-TARGET".into()),
        );
        input.config = Some(config);
        let created = store.create(input).unwrap();
        store.start_run(&created.id).unwrap();

        let metas = store.trigger_metadata().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].process_id, created.id);
        assert_eq!(metas[0].cron, created.cron);
        assert!(metas[0].enabled);
        assert!(metas[0].next_run_at.is_some());

        // The serialized export must not carry any in-TEE payload.
        let json = serde_json::to_string(&metas).unwrap();
        assert!(!json.contains("ULTRA-SENSITIVE-INSTRUCTION"));
        assert!(!json.contains("CONFIDENTIAL-TARGET"));
        assert!(!json.contains("prompt"));
        assert!(!json.contains("config"));
        assert!(!json.contains("summary"));
    }

    #[test]
    fn debug_redacts_prompt_and_config() {
        let mut config = serde_json::Map::new();
        config.insert("k".into(), serde_json::Value::String("hidden-cfg".into()));
        let record = ProcessRecord {
            id: "p1".into(),
            name: "n".into(),
            description: None,
            cron: "0 3 * * *".into(),
            prompt: "very-secret-instruction".into(),
            config: Some(config),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_run_at: None,
            next_run_at: None,
        };
        let rendered = format!("{record:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("very-secret-instruction"));
        assert!(!rendered.contains("hidden-cfg"));
    }

    #[test]
    fn sealed_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let store =
            ProcessStore::with_cipher(Arc::clone(&db), Some(Arc::new(SealCipher::new(&[1u8; 32]))));
        let created = store.create(new_process("locked")).unwrap();

        let other = ProcessStore::with_cipher(db, Some(Arc::new(SealCipher::new(&[2u8; 32]))));
        assert!(other.get(&created.id).is_err());
    }
}
