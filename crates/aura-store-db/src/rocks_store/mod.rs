//! `RocksDB` implementation of the Store trait.
//!
//! # Atomic Commit Protocol
//!
//! All mutations that involve more than one column family use [`WriteBatch`] to
//! guarantee **all-or-nothing** semantics.  `RocksDB` applies a `WriteBatch` as a
//! single atomic unit: either every put/delete in the batch is durably written,
//! or none of them are.
//!
//! The key multi-step operation is [`Store::append_entry_atomic`], which
//! performs four writes in one batch:
//!
//! 1. **Put** the serialised [`RecordEntry`] into the `record` column family.
//! 2. **Put** the updated `head_seq` into `agent_meta`.
//! 3. **Delete** the consumed inbox entry from the `inbox` column family.
//! 4. **Put** the advanced `inbox_head` cursor into `agent_meta`.
//!
//! Because these four operations share one `WriteBatch`, it is impossible to
//! observe a state where the record was written but the inbox was not advanced,
//! or vice-versa.  Transaction enqueue ([`Store::enqueue_tx`]) likewise batches
//! the inbox entry write with the tail-cursor update.
//!
//! # Failure Modes
//!
//! * **Partial writes are impossible** – the `WriteBatch` contract prevents
//!   them at the `RocksDB` level.
//! * **Sequence mismatch** – `append_entry_atomic` validates that `next_seq ==
//!   current_head + 1` before writing; a mismatch returns
//!   [`StoreError::SequenceMismatch`] without mutating state.
//! * **Disk-level failures** (e.g. full disk, storage corruption) may leave the
//!   WAL or SST files in an inconsistent state. `RocksDB`'s WAL replay can
//!   recover from crashes mid-write, but hardware-level corruption (bit-rot,
//!   torn sectors) may require restoring from backup.
//! * **`sync_writes`** controls whether each `WriteBatch` issues an `fsync`.
//!   When disabled, a process crash can lose committed batches that haven't
//!   been flushed to disk yet.

use crate::cf;
use crate::error::StoreError;
use crate::keys::{AgentMetaKey, InboxKey, KeyCodec, RecordKey};
use crate::seal::SealCipher;
use crate::store::ReadStore;
use aura_core_types::AgentStatus;
use aura_core_types::{
    AgentId, RecordEntry, RuntimeCapabilityInstall, Transaction, UserToolDefaults,
};
use rocksdb::{
    BlockBasedOptions, BoundColumnFamily, Cache, ColumnFamilyDescriptor, DBCompressionType,
    DBWithThreadMode, Direction, IteratorMode, MultiThreaded, Options, WriteBatch, WriteOptions,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, instrument};

/// `RocksDB`-based store implementation.
pub struct RocksStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
    sync_writes: bool,
    processing_claim_lock: Mutex<()>,
    /// Serializes the record-append read-modify-write so the
    /// `get_head_seq` / `assert_next_seq` pre-flight and the paired
    /// `WriteBatch` commit are atomic with respect to each other.
    ///
    /// Without this, two concurrent appenders for the same agent can
    /// both read head `N`, both pass the `next_seq == N + 1` check, and
    /// both commit `seq = N + 1` — the second silently overwrites the
    /// first (a lost update) and leaves `head = N + 1`. The scheduler's
    /// per-agent processing claim prevents most contention, but
    /// out-of-band system appends (e.g. the fleet's `SubagentSpawn`
    /// audit record written while the parent's turn is in flight) are
    /// not covered by that claim. Holding this lock across the
    /// check+commit turns the only observable concurrency failure into
    /// a clean [`StoreError::SequenceMismatch`] the caller can retry.
    append_lock: Mutex<()>,
    /// Optional value-sealing cipher (Swarm TEE phase 5).
    ///
    /// `Some` when the harness booted in `AURA_STATE_ENCRYPTION=sealed`
    /// mode: every content-bearing value (record entries, inbox
    /// transactions, runtime-capability snapshots, user tool defaults)
    /// is AES-256-GCM sealed before the `WriteBatch` and opened after
    /// reads. `None` is the legacy plaintext path, byte-for-byte
    /// identical to the historical on-disk format. Keys and pure-counter
    /// metadata (head/tail cursors, status byte, processing claims)
    /// stay plaintext in both modes — see [`crate::seal`] module docs.
    cipher: Option<Arc<SealCipher>>,
}

impl RocksStore {
    const BLOCK_CACHE_BYTES: usize = 64 * 1024 * 1024;

    /// Open or create a `RocksDB` store at the given path.
    ///
    /// # Errors
    /// Returns error if the database cannot be opened.
    pub fn open(path: impl AsRef<Path>, sync_writes: bool) -> Result<Self, StoreError> {
        Self::open_sealed(path, sync_writes, None)
    }

    /// Open or create a `RocksDB` store with optional value sealing.
    ///
    /// When `cipher` is `Some`, content-bearing values are encrypted at
    /// rest (sealed mode); when `None` this is exactly [`Self::open`].
    ///
    /// # Errors
    /// Returns error if the database cannot be opened.
    pub fn open_sealed(
        path: impl AsRef<Path>,
        sync_writes: bool,
        cipher: Option<Arc<SealCipher>>,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        debug!(?path, sealed = cipher.is_some(), "Opening RocksDB store");

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        // Define column families
        let cf_names = [
            cf::RECORD,
            cf::AGENT_META,
            cf::INBOX,
            cf::MEMORY_FACTS,
            cf::MEMORY_EVENTS,
            cf::MEMORY_PROCEDURES,
            cf::MEMORY_EVENT_INDEX,
            cf::MEMORY_CONFIG,
            cf::AGENT_SKILLS,
            cf::RUNTIME_CAPABILITIES,
            cf::USER_TOOL_DEFAULTS,
            cf::SECRETS,
            cf::PROCESSES,
            cf::PROCESS_RUNS,
        ];
        let block_cache = Cache::new_lru_cache(Self::BLOCK_CACHE_BYTES);
        let cf_descriptors: Vec<_> = cf_names
            .iter()
            .map(|name| {
                let cf_opts = Self::column_family_options(name, &block_cache);
                ColumnFamilyDescriptor::new(*name, cf_opts)
            })
            .collect();

        let db =
            DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, path, cf_descriptors)?;

        Ok(Self {
            db: Arc::new(db),
            sync_writes,
            processing_claim_lock: Mutex::new(()),
            append_lock: Mutex::new(()),
            cipher,
        })
    }

    /// Seal a serialized value when sealed mode is on; identity otherwise.
    fn seal_value(&self, plain: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        match &self.cipher {
            Some(cipher) => cipher
                .seal(&plain)
                .map_err(|e| StoreError::Serialization(format!("sealing value: {e}"))),
            None => Ok(plain),
        }
    }

    /// Open a sealed value when sealed mode is on; borrow-through otherwise.
    fn open_value<'a>(&self, bytes: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, StoreError> {
        match &self.cipher {
            Some(cipher) => cipher
                .open(bytes)
                .map(std::borrow::Cow::Owned)
                .map_err(|e| StoreError::Deserialization(format!("opening sealed value: {e}"))),
            None => Ok(std::borrow::Cow::Borrowed(bytes)),
        }
    }

    /// Get a column family handle.
    fn cf(&self, name: &str) -> Result<Arc<BoundColumnFamily<'_>>, StoreError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| StoreError::ColumnFamilyNotFound(name.to_string()))
    }

    /// Expose the underlying `RocksDB` handle for subsystems (e.g. memory store)
    /// that share the same database instance.
    #[must_use]
    pub const fn db_handle(&self) -> &Arc<DBWithThreadMode<MultiThreaded>> {
        &self.db
    }

    /// Create write options based on `sync_writes` setting.
    fn write_opts(&self) -> WriteOptions {
        let mut opts = WriteOptions::default();
        opts.set_sync(self.sync_writes);
        opts
    }

    fn column_family_options(name: &str, block_cache: &Cache) -> Options {
        let mut opts = Options::default();
        opts.set_block_based_table_factory(&Self::block_based_table_options(block_cache));

        if Self::should_compress_column_family(name) {
            opts.set_compression_type(DBCompressionType::Lz4);
        }

        opts
    }

    fn block_based_table_options(block_cache: &Cache) -> BlockBasedOptions {
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_cache(block_cache);
        block_opts.set_bloom_filter(10.0, false);
        block_opts.set_whole_key_filtering(true);
        block_opts.set_cache_index_and_filter_blocks(true);
        block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
        block_opts
    }

    fn should_compress_column_family(name: &str) -> bool {
        matches!(
            name,
            cf::RECORD
                | cf::INBOX
                | cf::MEMORY_FACTS
                | cf::MEMORY_EVENTS
                | cf::MEMORY_CONFIG
                | cf::MEMORY_PROCEDURES
                | cf::AGENT_SKILLS
                | cf::RUNTIME_CAPABILITIES
                | cf::USER_TOOL_DEFAULTS
                | cf::SECRETS
                | cf::PROCESSES
                | cf::PROCESS_RUNS
        )
    }

    /// Read a u64 value from agent metadata.
    fn read_meta_u64(&self, key: &AgentMetaKey) -> Result<u64, StoreError> {
        let cf = self.cf(cf::AGENT_META)?;
        let encoded_key = key.encode();

        match self.db.get_cf(&cf, &encoded_key)? {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| StoreError::Deserialization("invalid u64 bytes".to_string()))?;
                Ok(u64::from_be_bytes(arr))
            }
            None => Ok(0), // Default to 0 if not set
        }
    }

    fn runtime_capability_key(agent_id: AgentId) -> [u8; 32] {
        *agent_id.as_bytes()
    }

    /// Assert that `next_seq` is exactly `current_head + 1` for
    /// `agent_id`, returning [`StoreError::SequenceMismatch`] if not.
    ///
    /// Extracted so both `append_entry_direct_internal` and
    /// `append_entry_atomic_internal` use the same pre-flight check.
    fn assert_next_seq(&self, agent_id: AgentId, next_seq: u64) -> Result<(), StoreError> {
        let current_head = self.get_head_seq(agent_id)?;
        if next_seq != current_head + 1 {
            return Err(StoreError::SequenceMismatch {
                expected: current_head + 1,
                actual: next_seq,
            });
        }
        Ok(())
    }

    /// Add the serialized record-entry write and the head-sequence bump
    /// to `batch` for `(agent_id, next_seq)`.
    ///
    /// Extracted so both append paths produce bit-identical byte
    /// streams for the record + head-seq pair.
    fn append_record_and_head_to_batch(
        &self,
        batch: &mut WriteBatch,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
    ) -> Result<(), StoreError> {
        let cf_record = self.cf(cf::RECORD)?;
        let cf_meta = self.cf(cf::AGENT_META)?;

        let entry_bytes = self.seal_value(serde_json::to_vec(entry)?)?;
        let record_key = RecordKey::new(agent_id, next_seq);
        let head_seq_key = AgentMetaKey::head_seq(agent_id);

        batch.put_cf(&cf_record, record_key.encode(), entry_bytes);
        batch.put_cf(&cf_meta, head_seq_key.encode(), next_seq.to_be_bytes());
        Ok(())
    }

    /// Apply runtime-capability ledger mutations onto `batch`.
    ///
    /// When `clear_runtime_capabilities` is `true`, an explicit delete
    /// is queued. When `runtime_capabilities` is `Some`, the fresh
    /// payload is serialized and `put`. Ordering matches the original
    /// inline code: `clear` is added before `put`, so a simultaneous
    /// clear + set reduces to a single effective `put` (the new value
    /// wins within the batch — semantics preserved from the original
    /// `append_entry_*_internal` bodies).
    fn apply_runtime_capability_ops_to_batch(
        &self,
        batch: &mut WriteBatch,
        agent_id: AgentId,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
        clear_runtime_capabilities: bool,
    ) -> Result<(), StoreError> {
        let cf_runtime_capabilities = self.cf(cf::RUNTIME_CAPABILITIES)?;
        let capability_key = Self::runtime_capability_key(agent_id);

        if clear_runtime_capabilities {
            batch.delete_cf(&cf_runtime_capabilities, capability_key);
        }

        if let Some(runtime_capabilities) = runtime_capabilities {
            let capability_bytes = self.seal_value(serde_json::to_vec(runtime_capabilities)?)?;
            batch.put_cf(
                &cf_runtime_capabilities,
                Self::runtime_capability_key(agent_id),
                capability_bytes,
            );
        }
        Ok(())
    }

    fn append_entry_direct_internal(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
        clear_runtime_capabilities: bool,
    ) -> Result<(), StoreError> {
        let _append_guard = self
            .append_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.assert_next_seq(agent_id, next_seq)?;

        let mut batch = WriteBatch::default();
        self.append_record_and_head_to_batch(&mut batch, agent_id, next_seq, entry)?;
        self.apply_runtime_capability_ops_to_batch(
            &mut batch,
            agent_id,
            runtime_capabilities,
            clear_runtime_capabilities,
        )?;

        self.db.write_opt(batch, &self.write_opts())?;
        debug!("Record entry committed (direct)");
        Ok(())
    }

    fn append_entry_atomic_internal(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        dequeued_inbox_seq: u64,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
        clear_runtime_capabilities: bool,
    ) -> Result<(), StoreError> {
        let cf_meta = self.cf(cf::AGENT_META)?;
        let cf_inbox = self.cf(cf::INBOX)?;

        let _append_guard = self
            .append_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.assert_next_seq(agent_id, next_seq)?;

        let inbox_key = InboxKey::new(agent_id, dequeued_inbox_seq);
        let inbox_head_key = AgentMetaKey::inbox_head(agent_id);

        let mut batch = WriteBatch::default();
        self.append_record_and_head_to_batch(&mut batch, agent_id, next_seq, entry)?;
        // Inbox dequeue is the only operation that differs between the
        // direct and atomic append paths.
        batch.delete_cf(&cf_inbox, inbox_key.encode());
        batch.put_cf(
            &cf_meta,
            inbox_head_key.encode(),
            (dequeued_inbox_seq + 1).to_be_bytes(),
        );
        self.apply_runtime_capability_ops_to_batch(
            &mut batch,
            agent_id,
            runtime_capabilities,
            clear_runtime_capabilities,
        )?;

        self.db.write_opt(batch, &self.write_opts())?;

        debug!("Record entry committed atomically");
        Ok(())
    }

    fn processing_claim_key(agent_id: AgentId) -> Vec<u8> {
        AgentMetaKey::processing_claim(agent_id).encode()
    }
}

// Wave 2 T3: the legacy `impl Store for RocksStore` is split into the two
// narrower traits + the sealed marker so the type system enforces who is
// allowed to append to the record log (Invariant §10).
impl crate::store::sealed::Sealed for RocksStore {}

impl ReadStore for RocksStore {
    #[instrument(skip(self, tx), fields(agent_id = %tx.agent_id, hash = %tx.hash))]
    fn enqueue_tx(&self, tx: &Transaction) -> Result<(), StoreError> {
        let cf_inbox = self.cf(cf::INBOX)?;
        let cf_meta = self.cf(cf::AGENT_META)?;

        // Get current inbox tail
        let tail_key = AgentMetaKey::inbox_tail(tx.agent_id);
        let tail = self.read_meta_u64(&tail_key)?;

        // Create inbox key
        let inbox_key = InboxKey::new(tx.agent_id, tail);

        // Serialize transaction
        let tx_bytes = self.seal_value(serde_json::to_vec(tx)?)?;

        // Write batch: inbox entry + update tail
        let mut batch = WriteBatch::default();
        batch.put_cf(&cf_inbox, inbox_key.encode(), tx_bytes);
        batch.put_cf(&cf_meta, tail_key.encode(), (tail + 1).to_be_bytes());

        self.db.write_opt(batch, &self.write_opts())?;

        debug!(inbox_seq = tail, "Transaction enqueued");
        Ok(())
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn dequeue_tx(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<(crate::store::DequeueToken, Transaction)>, StoreError> {
        let cf_inbox = self.cf(cf::INBOX)?;

        // Get current inbox head and tail
        let head_key = AgentMetaKey::inbox_head(agent_id);
        let tail_key = AgentMetaKey::inbox_tail(agent_id);
        let head = self.read_meta_u64(&head_key)?;
        let tail = self.read_meta_u64(&tail_key)?;

        // Check if inbox is empty
        if head >= tail {
            debug!("Inbox empty");
            return Ok(None);
        }

        // Read the transaction at head
        let inbox_key = InboxKey::new(agent_id, head);
        let encoded_key = inbox_key.encode();

        if let Some(bytes) = self.db.get_cf(&cf_inbox, &encoded_key)? {
            let bytes = self.open_value(&bytes)?;
            let tx: Transaction = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Deserialization(e.to_string()))?;
            debug!(inbox_seq = head, "Transaction dequeued");
            let token = crate::store::DequeueToken { inbox_seq: head };
            Ok(Some((token, tx)))
        } else {
            Err(StoreError::InboxCorruption {
                agent_id,
                expected_seq: head,
            })
        }
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn get_head_seq(&self, agent_id: AgentId) -> Result<u64, StoreError> {
        let key = AgentMetaKey::head_seq(agent_id);
        self.read_meta_u64(&key)
    }

    #[instrument(skip(self), fields(agent_id = %agent_id, from_seq, limit))]
    fn scan_record(
        &self,
        agent_id: AgentId,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<RecordEntry>, StoreError> {
        let cf = self.cf(cf::RECORD)?;

        let start_key = RecordKey::scan_from(agent_id, from_seq);
        let end_key = RecordKey::scan_end(agent_id);

        let iter = self
            .db
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Forward));

        let mut entries = Vec::with_capacity(limit);

        for item in iter {
            let (key, value) = item?;

            if key.as_ref() >= end_key.as_slice() {
                break;
            }

            let record_key = RecordKey::decode(&key)?;

            if record_key.agent_id != agent_id {
                break;
            }

            let value = self.open_value(&value)?;
            let entry = serde_json::from_slice::<RecordEntry>(&value).map_err(|e| {
                StoreError::Deserialization(format!("record seq={}: {e}", record_key.seq))
            })?;
            entries.push(entry);

            if entries.len() >= limit {
                break;
            }
        }

        debug!(count = entries.len(), "Record scan complete");
        Ok(entries)
    }

    #[instrument(skip(self), fields(agent_id = %agent_id, from_seq, limit))]
    fn scan_record_descending(
        &self,
        agent_id: AgentId,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<RecordEntry>, StoreError> {
        if from_seq == 0 || limit == 0 {
            return Ok(Vec::new());
        }

        let cf = self.cf(cf::RECORD)?;
        let start_key = RecordKey::scan_from(agent_id, from_seq);
        let agent_start_key = RecordKey::scan_from(agent_id, 1);

        let iter = self
            .db
            .iterator_cf(&cf, IteratorMode::From(&start_key, Direction::Reverse));
        let mut entries = Vec::with_capacity(limit);

        for item in iter {
            let (key, value) = item?;

            if key.as_ref() < agent_start_key.as_slice() {
                break;
            }

            let record_key = RecordKey::decode(&key)?;
            if record_key.agent_id != agent_id {
                break;
            }

            let value = self.open_value(&value)?;
            let entry = serde_json::from_slice::<RecordEntry>(&value).map_err(|e| {
                StoreError::Deserialization(format!("record seq={}: {e}", record_key.seq))
            })?;
            entries.push(entry);

            if entries.len() >= limit {
                break;
            }
        }

        debug!(count = entries.len(), "Descending record scan complete");
        Ok(entries)
    }

    #[instrument(skip(self), fields(agent_id = %agent_id, seq))]
    fn get_record_entry(&self, agent_id: AgentId, seq: u64) -> Result<RecordEntry, StoreError> {
        let cf = self.cf(cf::RECORD)?;
        let key = RecordKey::new(agent_id, seq);

        match self.db.get_cf(&cf, key.encode())? {
            Some(bytes) => {
                let bytes = self.open_value(&bytes)?;
                let entry: RecordEntry = serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Deserialization(e.to_string()))?;
                Ok(entry)
            }
            None => Err(StoreError::RecordEntryNotFound(agent_id, seq)),
        }
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn get_agent_status(&self, agent_id: AgentId) -> Result<AgentStatus, StoreError> {
        let cf = self.cf(cf::AGENT_META)?;
        let key = AgentMetaKey::status(agent_id);

        match self.db.get_cf(&cf, key.encode())? {
            Some(bytes) => {
                if bytes.is_empty() {
                    return Ok(AgentStatus::default());
                }
                AgentStatus::from_byte(bytes[0])
                    .ok_or_else(|| StoreError::Deserialization("invalid agent status".to_string()))
            }
            None => Ok(AgentStatus::default()),
        }
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn get_runtime_capabilities(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<RuntimeCapabilityInstall>, StoreError> {
        let cf = self.cf(cf::RUNTIME_CAPABILITIES)?;
        let key = Self::runtime_capability_key(agent_id);

        match self.db.get_cf(&cf, key)? {
            Some(bytes) => {
                let bytes = self.open_value(&bytes)?;
                let capability_state = serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Deserialization(e.to_string()))?;
                Ok(Some(capability_state))
            }
            None => Ok(None),
        }
    }

    #[instrument(skip(self), fields(agent_id = %agent_id, ?status))]
    fn set_agent_status(&self, agent_id: AgentId, status: AgentStatus) -> Result<(), StoreError> {
        let cf = self.cf(cf::AGENT_META)?;
        let key = AgentMetaKey::status(agent_id);

        self.db
            .put_cf_opt(&cf, key.encode(), [status.as_byte()], &self.write_opts())?;
        Ok(())
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn has_pending_tx(&self, agent_id: AgentId) -> Result<bool, StoreError> {
        let head = self.read_meta_u64(&AgentMetaKey::inbox_head(agent_id))?;
        let tail = self.read_meta_u64(&AgentMetaKey::inbox_tail(agent_id))?;
        Ok(tail > head)
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn get_inbox_depth(&self, agent_id: AgentId) -> Result<u64, StoreError> {
        let head = self.read_meta_u64(&AgentMetaKey::inbox_head(agent_id))?;
        let tail = self.read_meta_u64(&AgentMetaKey::inbox_tail(agent_id))?;
        Ok(tail.saturating_sub(head))
    }

    #[instrument(skip(self), fields(user_id = %user_id))]
    fn get_user_tool_defaults(
        &self,
        user_id: &str,
    ) -> Result<Option<UserToolDefaults>, StoreError> {
        let cf = self.cf(cf::USER_TOOL_DEFAULTS)?;
        match self.db.get_cf(&cf, user_id.as_bytes())? {
            Some(bytes) => {
                let bytes = self.open_value(&bytes)?;
                let defaults = serde_json::from_slice::<UserToolDefaults>(&bytes)
                    .map_err(|e| StoreError::Deserialization(e.to_string()))?;
                Ok(Some(defaults))
            }
            None => Ok(None),
        }
    }

    #[instrument(skip(self, defaults), fields(user_id = %user_id))]
    fn put_user_tool_defaults(
        &self,
        user_id: &str,
        defaults: &UserToolDefaults,
    ) -> Result<(), StoreError> {
        let cf = self.cf(cf::USER_TOOL_DEFAULTS)?;
        let bytes = self.seal_value(serde_json::to_vec(defaults)?)?;
        self.db
            .put_cf_opt(&cf, user_id.as_bytes(), bytes, &self.write_opts())?;
        Ok(())
    }

    #[instrument(skip(self), fields(user_id = %user_id))]
    fn delete_user_tool_defaults(&self, user_id: &str) -> Result<(), StoreError> {
        let cf = self.cf(cf::USER_TOOL_DEFAULTS)?;
        self.db
            .delete_cf_opt(&cf, user_id.as_bytes(), &self.write_opts())?;
        Ok(())
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn try_claim_agent_processing(&self, agent_id: AgentId) -> Result<bool, StoreError> {
        let _guard = self
            .processing_claim_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cf = self.cf(cf::AGENT_META)?;
        let key = Self::processing_claim_key(agent_id);

        if self.db.get_cf(&cf, &key)?.is_some() {
            return Ok(false);
        }

        self.db.put_cf_opt(&cf, key, [1_u8], &self.write_opts())?;
        Ok(true)
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn release_agent_processing(&self, agent_id: AgentId) -> Result<(), StoreError> {
        let _guard = self
            .processing_claim_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cf = self.cf(cf::AGENT_META)?;
        let key = Self::processing_claim_key(agent_id);
        self.db.delete_cf_opt(&cf, key, &self.write_opts())?;
        Ok(())
    }

    #[instrument(skip(self), fields(agent_id = %agent_id))]
    fn is_agent_processing(&self, agent_id: AgentId) -> Result<bool, StoreError> {
        let cf = self.cf(cf::AGENT_META)?;
        let key = Self::processing_claim_key(agent_id);
        Ok(self.db.get_cf(&cf, key)?.is_some())
    }
}

impl crate::store::WriteStore for RocksStore {
    #[instrument(skip(self, entry), fields(agent_id = %agent_id, seq = next_seq))]
    fn append_entry_atomic(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        dequeued_inbox_seq: u64,
    ) -> Result<(), StoreError> {
        self.append_entry_atomic_internal(
            agent_id,
            next_seq,
            entry,
            dequeued_inbox_seq,
            None,
            false,
        )
    }

    #[instrument(skip(self, entry, runtime_capabilities), fields(agent_id = %agent_id, seq = next_seq))]
    fn append_entry_dequeued_with_runtime_capabilities(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        token: crate::store::DequeueToken,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
        clear_runtime_capabilities: bool,
    ) -> Result<(), StoreError> {
        self.append_entry_atomic_internal(
            agent_id,
            next_seq,
            entry,
            token.inbox_seq(),
            runtime_capabilities,
            clear_runtime_capabilities,
        )
    }

    #[instrument(skip(self, entry), fields(agent_id = %agent_id, seq = next_seq))]
    fn append_entry_direct(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
    ) -> Result<(), StoreError> {
        self.append_entry_direct_internal(agent_id, next_seq, entry, None, false)
    }

    #[instrument(skip(self, entry, runtime_capabilities), fields(agent_id = %agent_id, seq = next_seq))]
    fn append_entry_direct_with_runtime_capabilities(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        runtime_capabilities: Option<&RuntimeCapabilityInstall>,
        clear_runtime_capabilities: bool,
    ) -> Result<(), StoreError> {
        self.append_entry_direct_internal(
            agent_id,
            next_seq,
            entry,
            runtime_capabilities,
            clear_runtime_capabilities,
        )
    }

    #[instrument(skip(self, entries), fields(agent_id = %agent_id, base_seq, count = entries.len()))]
    fn append_entries_batch(
        &self,
        agent_id: AgentId,
        base_seq: u64,
        entries: &[RecordEntry],
    ) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }

        let cf_record = self.cf(cf::RECORD)?;
        let cf_meta = self.cf(cf::AGENT_META)?;

        let _append_guard = self
            .append_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_head = self.get_head_seq(agent_id)?;
        if base_seq != current_head + 1 {
            return Err(StoreError::SequenceMismatch {
                expected: current_head + 1,
                actual: base_seq,
            });
        }

        let mut batch = WriteBatch::default();
        let head_seq_key = AgentMetaKey::head_seq(agent_id);

        for (i, entry) in entries.iter().enumerate() {
            let seq = base_seq + i as u64;
            let entry_bytes = self.seal_value(serde_json::to_vec(entry)?)?;
            let record_key = RecordKey::new(agent_id, seq);
            batch.put_cf(&cf_record, record_key.encode(), entry_bytes);
        }

        let last_seq = base_seq + entries.len() as u64 - 1;
        batch.put_cf(&cf_meta, head_seq_key.encode(), last_seq.to_be_bytes());

        self.db.write_opt(batch, &self.write_opts())?;

        debug!(last_seq, "Batch record entries committed");
        Ok(())
    }
}

// --------------------------------------------------------------------
// Fault-injection helpers (Wave 7 / T2 — invariant_atomicity suite).
//
// These helpers are gated on `#[cfg(any(test, feature = "test-support"))]`
// so they are never compiled into non-test builds unless a caller
// explicitly opts into the `test-support` feature. They exist solely to
// exercise Invariant §10 against `RocksStore::append_entry_atomic` —
// specifically to prove that a record-write and the paired inbox-delete
// either both commit or both roll back, under every simulated fault
// point.
// --------------------------------------------------------------------

/// Fault point for [`RocksStore::append_entry_atomic_with_fault`].
///
/// See [`RocksStore::append_entry_atomic_with_fault`] for semantics.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAt {
    /// Fail the call before issuing the `WriteBatch`. Expected outcome:
    /// no record is appended and no inbox slot is deleted.
    BeforeBatchWrite,
    /// Fail the call **after** the `WriteBatch` commits. Expected
    /// outcome: the record is appended **and** the inbox slot is
    /// deleted (the atomic commit is durable even though the caller
    /// observes an error).
    AfterBatchWrite,
    /// Deliberately write the record entry in one `WriteBatch`, fail
    /// before queueing the paired inbox delete, and return an error.
    /// This is the "broken non-atomic path" the test uses to prove
    /// that the real atomic path is strictly required by Invariant §10.
    /// Observing partial state here is expected and documents the
    /// failure mode that `append_entry_atomic` prevents.
    InsideBatch,
}

#[cfg(any(test, feature = "test-support"))]
impl RocksStore {
    /// Fault-injected variant of `append_entry_atomic`. See [`FaultAt`].
    ///
    /// # Errors
    /// Always returns an error in every fault mode so callers can assert
    /// both "the call failed" and the resulting store state in a single
    /// shot.
    #[allow(clippy::too_many_arguments)]
    pub fn append_entry_atomic_with_fault(
        &self,
        agent_id: AgentId,
        next_seq: u64,
        entry: &RecordEntry,
        dequeued_inbox_seq: u64,
        fault: FaultAt,
    ) -> Result<(), StoreError> {
        // Fault helper reuses `StoreError::InvalidKey` as a general-purpose
        // error carrier for injected faults so we don't have to grow the
        // public `StoreError` enum for test-only purposes. Callers assert
        // on the `injected fault:` prefix.
        let fault_err =
            |tag: &str| -> StoreError { StoreError::InvalidKey(format!("injected fault: {tag}")) };
        match fault {
            FaultAt::BeforeBatchWrite => Err(fault_err("BeforeBatchWrite")),
            FaultAt::AfterBatchWrite => {
                // Commit the full atomic batch first, then synthesize a
                // caller-side failure. This simulates e.g. a network
                // error after RocksDB has already fsynced.
                self.append_entry_atomic_internal(
                    agent_id,
                    next_seq,
                    entry,
                    dequeued_inbox_seq,
                    None,
                    false,
                )?;
                Err(fault_err("AfterBatchWrite"))
            }
            FaultAt::InsideBatch => {
                // Deliberately non-atomic: write the record entry in
                // one batch, then bail before queueing the inbox
                // delete. The assertion in the paired test is that
                // this path produces *partial* state — which is why
                // the real `append_entry_atomic` uses a single
                // `WriteBatch` containing both the record put and the
                // inbox delete.
                let cf_record = self.cf(cf::RECORD)?;
                let cf_meta = self.cf(cf::AGENT_META)?;

                let current_head = self.get_head_seq(agent_id)?;
                if next_seq != current_head + 1 {
                    return Err(StoreError::SequenceMismatch {
                        expected: current_head + 1,
                        actual: next_seq,
                    });
                }

                let entry_bytes = self.seal_value(serde_json::to_vec(entry)?)?;
                let record_key = RecordKey::new(agent_id, next_seq);
                let head_seq_key = AgentMetaKey::head_seq(agent_id);

                let mut batch = WriteBatch::default();
                batch.put_cf(&cf_record, record_key.encode(), entry_bytes);
                batch.put_cf(&cf_meta, head_seq_key.encode(), next_seq.to_be_bytes());
                self.db.write_opt(batch, &self.write_opts())?;

                // Intentionally no inbox delete — this is the bug.
                Err(fault_err("InsideBatch (broken non-atomic path)"))
            }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_concurrent;
