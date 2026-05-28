//! Tool-proposal processing.
//!
//! Single-proposal ([`Kernel::process_tool_proposal`]) and batch
//! ([`Kernel::process_tools`]) paths both:
//! 1. Route each proposal through the full
//!    `Policy::check_with_runtime_capabilities_verdict` pipeline
//!    (Invariant §4).
//! 2. Resolve any live `ask` prompt surfaced by
//!    [`crate::PolicyVerdict::RequireApproval`].
//! 3. Execute approved tools via `ExecutorRouter`.
//! 4. Build a `RecordEntry` with the proposal set, decision, actions,
//!    and effects attached (Invariant §5).
//!
//! The batch path additionally reserves a contiguous sequence range and
//! writes all entries via `append_entries_batch` for atomicity.
//!
//! The implementation is split for readability:
//! - [`shared`] — helpers used by both paths (`execute_with_timeout`,
//!   `kernel_proposal_from_tool_proposal`, `resolve_live_ask_verdict`,
//!   and the unified `record_entry_for_tool_outcome` builder).
//! - [`single`] — the single-proposal flow.
//! - [`batch`] — the batch flow with atomic append.

mod batch;
mod shared;
mod single;

use super::{Kernel, ProcessResult};
use aura_core::{ContextHash, ToolProposal, Transaction};
use tracing::instrument;

impl Kernel {
    #[instrument(skip(self, tx), fields(seq))]
    pub(super) async fn process_tool_proposal(
        &self,
        tx: &Transaction,
        seq: u64,
        context_hash: ContextHash,
    ) -> Result<ProcessResult, crate::KernelError> {
        single::process_one(self, tx, seq, context_hash).await
    }

    /// Process a batch of tool proposals, executing approved tools in parallel.
    ///
    /// # Errors
    /// Returns error if serialization, execution, or storage fails.
    pub async fn process_tools(
        &self,
        tool_proposals: Vec<ToolProposal>,
    ) -> Result<Vec<ProcessResult>, crate::KernelError> {
        batch::process_many(self, tool_proposals).await
    }
}
