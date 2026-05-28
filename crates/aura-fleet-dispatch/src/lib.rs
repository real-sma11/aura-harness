//! # aura-fleet-dispatch
//!
//! Layer: fleet
//!
//! Takes a stream of [`AgentJob`] items and routes each one into the
//! correct [`aura_fleet_spawn::FleetSpawner::spawn`] call.
//!
//! ## Phase 7b scope
//!
//! The dispatcher now consumes jobs from the
//! [`aura_fleet_mailbox::Mailbox`] **and** still supports the
//! direct task-tool path where one job is spawned per tool call.
//! Each job carries its resolved [`SpawnMode`] — `Wait`, `Detached`,
//! or `Batch` — and the dispatcher does **not** await detached or
//! batch handles. It returns the [`SpawnHandle`] to the caller (or
//! drops it into the registry-managed background pool) and moves
//! on to the next job.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - The dispatcher is **stateless** — every [`AgentJob`] is
//!   converted into a [`SpawnRequest`] and handed to the shared
//!   [`FleetSpawner`]. Concurrency / quota / dedupe lives in the
//!   spawner.
//! - The dispatcher **does not enqueue or persist jobs**. The
//!   mailbox crate owns enqueue + backpressure; the dispatcher
//!   only consumes.
//!
//! ## Failure modes
//!
//! - [`DispatchError::Spawn`] — the spawner rejected a job.
//! - [`DispatchError::Lagged`] — the input stream surfaced a
//!   `Lagged` error from a `tokio::sync::broadcast` (reserved for
//!   Phase 7b mailbox wiring).

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::sync::Arc;

use aura_core::SubagentResult;
use aura_core_modes::SpawnMode;
use aura_fleet_spawn::{FleetSpawner, SpawnError, SpawnHandle, SpawnRequest};
use futures_util::{Stream, StreamExt};
use thiserror::Error;
use tracing::instrument;

/// A job the dispatcher routes into a spawn call.
///
/// Wraps a single [`SpawnRequest`] plus the resolved [`SpawnMode`]
/// — `Wait`, `Detached`, or `Batch`. Phase 7b also threads a
/// per-job `priority` and `deadline` placeholder which today are
/// reserved for future scheduling work.
#[derive(Debug)]
pub struct AgentJob {
    /// The spawn request derived from a parent tool call.
    pub request: SpawnRequest,
    /// Resolved spawn mode (Wait / Detached / Batch).
    pub mode: SpawnMode,
}

/// Errors surfaced by [`FleetDispatcher::run`].
#[derive(Debug, Error)]
pub enum DispatchError {
    /// Underlying spawner rejected the job.
    #[error("dispatch failed: {0}")]
    Spawn(#[from] SpawnError),

    /// Input stream signalled a lag (reserved for Phase 7b
    /// broadcast wiring).
    #[error("dispatch input stream lagged: {0}")]
    Lagged(String),
}

/// Phase 7a dispatcher — wraps a shared [`FleetSpawner`] and
/// streams jobs through `spawn`.
pub struct FleetDispatcher {
    spawner: Arc<FleetSpawner>,
}

impl FleetDispatcher {
    /// Construct a dispatcher around an existing spawner.
    #[must_use]
    pub fn new(spawner: Arc<FleetSpawner>) -> Self {
        Self { spawner }
    }

    /// Drain a stream of [`AgentJob`] items, awaiting each spawn
    /// to completion. The dispatcher returns the [`SubagentResult`]
    /// for each job in input order. A single spawn failure aborts
    /// the loop and surfaces the [`DispatchError`].
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::Spawn`] on the first spawner
    /// rejection.
    #[instrument(skip(self, jobs))]
    pub async fn run<S>(&self, mut jobs: S) -> Result<Vec<SubagentResult>, DispatchError>
    where
        S: Stream<Item = AgentJob> + Unpin + Send,
    {
        let mut results = Vec::new();
        while let Some(job) = jobs.next().await {
            match self.spawn_one(job).await? {
                SpawnHandle::Completed(result) => results.push(result),
                // Detached / Batch jobs run in the background; the
                // dispatcher's job here is just to start them.
                SpawnHandle::Detached(_) | SpawnHandle::Batch(_) => {}
            }
        }
        Ok(results)
    }

    /// Spawn a single job. Useful for the task-tool adapter that
    /// constructs one job per tool call without building a stream.
    ///
    /// # Errors
    ///
    /// Surfaces any [`SpawnError`] from the underlying spawner.
    #[instrument(skip(self, job))]
    pub async fn spawn_one(&self, job: AgentJob) -> Result<SpawnHandle, DispatchError> {
        let handle = self.spawner.spawn(job.request, job.mode).await?;
        Ok(handle)
    }
}
