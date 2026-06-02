//! [`KernelDomainGateway`] struct, constructor, and private
//! `record_request` / `record_response` helpers.
//!
//! The trait `impl` lives in [`super::routes`] so this file stays scoped
//! to "what is the gateway and how does it record".

use std::sync::Arc;

use aura_agent_kernel::Kernel;
use aura_core_types::{SystemKind, Transaction, TransactionType};
use aura_tools::domain_tools::DomainApi;
use serde_json::{json, Value};
use tracing::error;

/// Gateway implementing [`DomainApi`] by routing every mutation
/// through [`Kernel::process_direct`] so the kernel's record log
/// captures the request snapshot and response outcome.
pub struct KernelDomainGateway {
    pub(super) inner: Arc<dyn DomainApi>,
    pub(super) kernel: Arc<Kernel>,
}

impl KernelDomainGateway {
    /// Wrap `inner` so every mutating call first records a request
    /// entry and then records the response (success or failure).
    #[must_use]
    pub fn new(inner: Arc<dyn DomainApi>, kernel: Arc<Kernel>) -> Self {
        Self { inner, kernel }
    }

    /// Record a "request" entry before an outbound mutating call.
    ///
    /// Returns `()` on success; errors are logged at `error!` so the
    /// caller can still attempt the outbound call. We deliberately do
    /// not propagate the recording error: per §3 / §2, a failure to
    /// record is not a reason to refuse the primary operation, but
    /// the underlying store failure is still surfaced via `tracing`
    /// so operators can detect silent drift.
    pub(super) async fn record_request(&self, method: &'static str, args: Value) {
        let payload = json!({
            "system_kind": SystemKind::DomainMutation,
            "phase": "request",
            "method": method,
            "args": args,
        });
        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                error!(method, error = %e, "KernelDomainGateway: failed to serialize request snapshot");
                return;
            }
        };
        let tx =
            Transaction::new_chained(self.kernel.agent_id, TransactionType::System, bytes, None);
        if let Err(e) = self.kernel.process_direct(tx).await {
            error!(method, error = %e, "KernelDomainGateway: failed to record domain mutation request");
        }
    }

    /// Record a "response" entry after an outbound mutating call.
    ///
    /// `ok` carries whether the inner call succeeded; when it failed
    /// the error message is captured verbatim in the payload.
    pub(super) async fn record_response(
        &self,
        method: &'static str,
        ok: bool,
        error_msg: Option<String>,
    ) {
        let payload = json!({
            "system_kind": SystemKind::DomainMutation,
            "phase": "response",
            "method": method,
            "status": if ok { "ok" } else { "error" },
            "error": error_msg,
        });
        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                error!(method, error = %e, "KernelDomainGateway: failed to serialize response snapshot");
                return;
            }
        };
        let tx =
            Transaction::new_chained(self.kernel.agent_id, TransactionType::System, bytes, None);
        if let Err(e) = self.kernel.process_direct(tx).await {
            error!(method, error = %e, "KernelDomainGateway: failed to record domain mutation response");
        }
    }
}
