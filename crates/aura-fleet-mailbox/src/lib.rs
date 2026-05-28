//! # aura-fleet-mailbox
//!
//! Layer: fleet
//!
//! Bounded multi-producer single-consumer mailbox over
//! [`tokio::sync::mpsc`]. Any source (the task tool, the SDK, the
//! daemon's RPC surface) pushes an [`aura_fleet_dispatch::AgentJob`]
//! into the mailbox; the daemon's run loop drains it on the other
//! side via [`MailboxReceiver`].
//!
//! ## Backpressure policy
//!
//! The channel is bounded. Default capacity is
//! [`DEFAULT_MAILBOX_CAPACITY`] (1024). When full, producers using
//! [`MailboxSender::send_with_deadline`] block the calling task up to
//! the supplied deadline; if the deadline expires before a slot frees
//! the call surfaces [`MailboxError::Backpressured`]. Producers that
//! must never block use [`MailboxSender::try_send`] and handle the
//! `Full` variant themselves.
//!
//! `MailboxSender::send` (no deadline) blocks indefinitely — useful
//! for top-level sources where the slow consumer is the symptom that
//! needs surfacing, not the cause.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - Bounded capacity: the channel never grows beyond
//!   [`MailboxConfig::capacity`] queued items. This caps memory and
//!   bounds the worst-case dispatch latency for any single job.
//! - Once dropped, the receiver causes every subsequent producer
//!   send to return [`MailboxError::Closed`]; a drained receiver is
//!   indistinguishable from a closed receiver to producers.
//! - No silent drops: every backpressure / closed condition surfaces
//!   as a typed error variant.
//!
//! ## Assumptions
//!
//! - The mailbox is owned by the daemon; producers obtain
//!   [`MailboxSender`] clones via [`Mailbox::sender`].
//! - The consumer side drives [`MailboxReceiver::recv`] in a single
//!   task; concurrent consumers are NOT supported (mpsc semantics).
//!
//! ## Failure modes
//!
//! - [`MailboxError::Backpressured`] — full channel and the deadline
//!   expired. The caller may retry or surface the error.
//! - [`MailboxError::Closed`] — the receiver was dropped or its
//!   owning task exited. The producer should stop pushing.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::time::Duration;

use aura_fleet_dispatch::AgentJob;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Default capacity for [`Mailbox::with_default_capacity`].
pub const DEFAULT_MAILBOX_CAPACITY: usize = 1024;

/// Static configuration for a [`Mailbox`].
#[derive(Debug, Clone, Copy)]
pub struct MailboxConfig {
    /// Maximum number of queued jobs.
    pub capacity: usize,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_MAILBOX_CAPACITY,
        }
    }
}

/// Errors surfaced by [`MailboxSender`].
#[derive(Debug, Error)]
pub enum MailboxError {
    /// Send-with-deadline timed out because the channel was full.
    #[error("mailbox: channel full (capacity {capacity}); send deadline {deadline_ms}ms expired")]
    Backpressured {
        /// Configured channel capacity.
        capacity: usize,
        /// Deadline the caller supplied.
        deadline_ms: u64,
    },
    /// Receiver was dropped (mailbox closed).
    #[error("mailbox: receiver dropped — no consumer available")]
    Closed,
}

/// Clone-able producer handle.
#[derive(Debug, Clone)]
pub struct MailboxSender {
    inner: mpsc::Sender<AgentJob>,
    capacity: usize,
}

impl MailboxSender {
    /// Send a job, blocking the calling task until a slot frees.
    /// Returns [`MailboxError::Closed`] if the receiver has been
    /// dropped.
    ///
    /// # Errors
    ///
    /// See [`MailboxError::Closed`].
    pub async fn send(&self, job: AgentJob) -> Result<(), MailboxError> {
        self.inner.send(job).await.map_err(|_| MailboxError::Closed)
    }

    /// Try to send without blocking. Returns
    /// [`MailboxError::Backpressured`] if the channel is full or
    /// [`MailboxError::Closed`] if the receiver was dropped.
    ///
    /// # Errors
    ///
    /// See [`MailboxError`].
    pub fn try_send(&self, job: AgentJob) -> Result<(), MailboxError> {
        self.inner.try_send(job).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => MailboxError::Backpressured {
                capacity: self.capacity,
                deadline_ms: 0,
            },
            mpsc::error::TrySendError::Closed(_) => MailboxError::Closed,
        })
    }

    /// Send with a wall-clock deadline. Blocks the calling task up
    /// to `deadline`; if the deadline expires before a slot frees
    /// the call returns [`MailboxError::Backpressured`].
    ///
    /// # Errors
    ///
    /// See [`MailboxError`].
    pub async fn send_with_deadline(
        &self,
        job: AgentJob,
        deadline: Duration,
    ) -> Result<(), MailboxError> {
        match timeout(deadline, self.inner.send(job)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(MailboxError::Closed),
            Err(_) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let deadline_ms = deadline.as_millis() as u64;
                warn!(
                    capacity = self.capacity,
                    deadline_ms, "mailbox: send deadline expired (Backpressured)"
                );
                Err(MailboxError::Backpressured {
                    capacity: self.capacity,
                    deadline_ms,
                })
            }
        }
    }

    /// Capacity of the underlying bounded channel.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Consumer side of the mailbox. Single-owner — cloning is NOT
/// supported (mpsc semantics).
#[derive(Debug)]
pub struct MailboxReceiver {
    inner: mpsc::Receiver<AgentJob>,
}

impl MailboxReceiver {
    /// Block until a job arrives or the channel is closed.
    /// Returns `None` when every [`MailboxSender`] handle has been
    /// dropped AND the channel is drained.
    pub async fn recv(&mut self) -> Option<AgentJob> {
        self.inner.recv().await
    }

    /// Try to dequeue without blocking.
    pub fn try_recv(&mut self) -> Option<AgentJob> {
        self.inner.try_recv().ok()
    }
}

/// Producer/consumer pair builder for a single bounded mailbox.
///
/// Owners typically destructure into `(sender, receiver)` once and
/// pass the sender into the surface / RPC code while the receiver
/// runs inside the daemon's event loop.
#[derive(Debug)]
pub struct Mailbox {
    sender: MailboxSender,
    receiver: MailboxReceiver,
}

impl Mailbox {
    /// Construct a mailbox with the default capacity.
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::with_config(MailboxConfig::default())
    }

    /// Construct a mailbox with an explicit config.
    #[must_use]
    pub fn with_config(config: MailboxConfig) -> Self {
        let (tx, rx) = mpsc::channel::<AgentJob>(config.capacity);
        debug!(capacity = config.capacity, "mailbox: created");
        Self {
            sender: MailboxSender {
                inner: tx,
                capacity: config.capacity,
            },
            receiver: MailboxReceiver { inner: rx },
        }
    }

    /// Cheap-clone the sender handle. Producers each take their own.
    #[must_use]
    pub fn sender(&self) -> MailboxSender {
        self.sender.clone()
    }

    /// Split into owned (sender, receiver) halves. The receiver is
    /// the single consumer; the sender may be further cloned.
    #[must_use]
    pub fn into_parts(self) -> (MailboxSender, MailboxReceiver) {
        (self.sender, self.receiver)
    }
}

// Integration tests live in `tests/mailbox_backpressure.rs` so they
// can construct a real `aura_fleet_dispatch::AgentJob` via the
// upstream `aura_fleet_spawn` test helpers; an inline `#[cfg(test)]`
// module here would otherwise have to drag `aura-fleet-spawn` into
// the crate's dev-deps and create a layering loop with
// `aura-fleet-dispatch`.
