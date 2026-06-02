//! [`SpawnHandle`] taxonomy — outcomes returned from
//! [`crate::FleetSpawner::spawn`] / `spawn_batch` per [`SpawnMode`].
//!
//! Three concrete handle shapes:
//!
//! - [`SpawnHandle::Completed`] — synchronous [`SpawnMode::Wait`]
//!   result; the child ran to completion in-call.
//! - [`SpawnHandle::Detached`] — child is running in a background
//!   `tokio::spawn`; the parent gets an [`AgentId`] plus an optional
//!   `oneshot::Receiver<SubagentResult>` it MAY await later or drop.
//! - [`SpawnHandle::Batch`] — batch of N children joined per the
//!   spec's [`JoinPolicy`]; the parent gets a [`BatchSpawn`] handle
//!   it `await`s on.

use aura_core_modes::{JoinPolicy, SpawnMode};
use aura_core_types::{AgentId, SubagentResult};
use tokio::sync::oneshot;

use crate::SpawnError;

/// Per-call outcome of [`crate::FleetSpawner::spawn`].
#[derive(Debug)]
pub enum SpawnHandle {
    /// [`SpawnMode::Wait`] — child ran to completion in-call.
    Completed(SubagentResult),
    /// [`SpawnMode::Detached`] — child is running in the background.
    Detached(DetachedSpawn),
    /// [`SpawnMode::Batch`] — N children joined per [`JoinPolicy`].
    Batch(BatchSpawn),
}

impl SpawnHandle {
    /// Convenience: returns the spawn mode this handle was created
    /// for.
    #[must_use]
    pub fn mode(&self) -> SpawnMode {
        match self {
            SpawnHandle::Completed(_) => SpawnMode::Wait,
            SpawnHandle::Detached(_) => SpawnMode::Detached,
            SpawnHandle::Batch(_) => SpawnMode::Batch,
        }
    }
}

/// [`SpawnMode::Detached`] outcome.
///
/// The parent receives the child [`AgentId`] immediately. The
/// `result_rx` field is `Some(_)` for the original caller (it
/// observes the child's [`SubagentResult`] when ready) and `None`
/// for dedupe-cloned handles — see [`crate::ParentLeaseRegistry`].
#[derive(Debug)]
pub struct DetachedSpawn {
    /// Stable child agent id.
    pub agent_id: AgentId,
    /// Optional one-shot receiver for the child's final result.
    /// `None` indicates the parent has no observable channel (the
    /// child is purely fire-and-forget OR this is a dedupe-cloned
    /// handle — the original observer is somewhere else).
    pub result_rx: Option<oneshot::Receiver<SubagentResult>>,
}

impl DetachedSpawn {
    /// Wait for the child's final result. Returns `None` if the
    /// underlying oneshot channel was dropped (child crashed
    /// without producing a result OR this is a dedupe-cloned
    /// handle).
    pub async fn join(self) -> Option<SubagentResult> {
        match self.result_rx {
            Some(rx) => rx.await.ok(),
            None => None,
        }
    }
}

/// [`SpawnMode::Batch`] outcome aggregated per [`JoinPolicy`].
#[derive(Debug)]
pub struct BatchSpawn {
    /// Child agent ids in spawn order. Always populated; for
    /// `JoinPolicy::Abandon` this is the only useful field after
    /// [`BatchSpawn::join`] returns.
    pub agent_ids: Vec<AgentId>,
    /// Resolved join policy for this batch.
    pub policy: JoinPolicy,
    /// Internal state machine — see [`BatchSpawn::join`].
    pub(crate) inner: BatchInner,
}

#[derive(Debug)]
pub(crate) enum BatchInner {
    /// `All` — collected futures one per child. Resolution is in
    /// spawn order.
    All(Vec<tokio::task::JoinHandle<Result<SubagentResult, SpawnError>>>),
    /// `Any` — collected futures + per-child cancel tokens. First
    /// success wins; losers are cancelled and their results dropped.
    Any {
        /// Child futures (parallel to `cancellations`).
        children: Vec<tokio::task::JoinHandle<Result<SubagentResult, SpawnError>>>,
        /// Per-child cancellation tokens used to short-circuit the
        /// losers.
        cancellations: Vec<tokio_util::sync::CancellationToken>,
    },
    /// `Abandon` — children were spawned + orphaned; no observable
    /// state remains.
    Abandon,
}

/// Aggregated outcome of a [`BatchSpawn::join`] call.
#[derive(Debug)]
pub enum BatchOutcome {
    /// `JoinPolicy::All` outcome: `Vec` of per-child results in
    /// spawn order. Failed children appear as `Err(_)`; siblings
    /// keep running.
    All(Vec<Result<SubagentResult, SpawnError>>),
    /// `JoinPolicy::Any` outcome: first success or every error.
    Any(Result<SubagentResult, Vec<SpawnError>>),
    /// `JoinPolicy::Abandon` outcome: list of orphaned child ids
    /// the parent may pick up later via `aura agents inspect`.
    Abandoned(Vec<AgentId>),
}

impl BatchSpawn {
    /// Resolve the batch per its [`JoinPolicy`].
    pub async fn join(self) -> BatchOutcome {
        let agent_ids = self.agent_ids;
        match self.inner {
            BatchInner::All(handles) => {
                let mut out = Vec::with_capacity(handles.len());
                for handle in handles {
                    match handle.await {
                        Ok(Ok(result)) => out.push(Ok(result)),
                        Ok(Err(err)) => out.push(Err(err)),
                        Err(join_err) => {
                            out.push(Err(SpawnError::Child(crate::ChildRunError::Internal(
                                format!("batch child join: {join_err}"),
                            ))));
                        }
                    }
                }
                BatchOutcome::All(out)
            }
            BatchInner::Any {
                mut children,
                cancellations,
            } => {
                let mut errors = Vec::new();
                while !children.is_empty() {
                    // `select_all` requires Unpin futures; JoinHandle
                    // is Unpin so we can use the simple variant.
                    let (next, idx, rest) = futures_util::future::select_all(children).await;
                    children = rest;
                    match next {
                        Ok(Ok(result)) => {
                            // Cancel every still-running sibling.
                            for token in &cancellations {
                                token.cancel();
                            }
                            // Drain the rest in the background so we
                            // don't leak tasks; ignore their results.
                            for handle in children {
                                tokio::spawn(async move {
                                    let _ = handle.await;
                                });
                            }
                            let _ = (idx, agent_ids);
                            return BatchOutcome::Any(Ok(result));
                        }
                        Ok(Err(err)) => errors.push(err),
                        Err(join_err) => {
                            errors.push(SpawnError::Child(crate::ChildRunError::Internal(
                                format!("batch child join: {join_err}"),
                            )));
                        }
                    }
                }
                BatchOutcome::Any(Err(errors))
            }
            BatchInner::Abandon => BatchOutcome::Abandoned(agent_ids),
        }
    }

    /// Snapshot of the orphan agent ids for a `JoinPolicy::Abandon`
    /// batch. Returns the empty slice for other policies (use
    /// [`BatchSpawn::join`] for those).
    #[must_use]
    pub fn abandoned(&self) -> &[AgentId] {
        match self.inner {
            BatchInner::Abandon => &self.agent_ids,
            _ => &[],
        }
    }
}
