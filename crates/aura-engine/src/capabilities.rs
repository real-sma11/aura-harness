//! Runtime-capability recording helper used by the automaton bridge.
//!
//! When a dev-loop or task-run automaton boots, the bridge records a
//! `SystemKind::CapabilityInstall` entry that pins the
//! `installed_tools` + `installed_integrations` it negotiated with the
//! upstream `RuntimeRequest`. That entry preserves the per-run
//! capability inventory in the audit log so post-hoc debugging can tell
//! "this dev-loop saw exactly these tools at startup" from a record-log
//! scan.

use aura_core::{
    InstalledIntegrationDefinition, InstalledToolCapability, InstalledToolDefinition,
    RuntimeCapabilityInstall, SystemKind, Transaction, TransactionType,
};
use aura_kernel::{Kernel, KernelError};

/// Record a `SessionStart` followed by a `CapabilityInstall` system
/// entry on the supplied kernel. The bridge calls this for every
/// automaton kickoff before handing control to the model loop, so the
/// audit log carries an unambiguous "session began" + "tools/integrations
/// at session start" pair.
pub async fn record_runtime_capabilities(
    kernel: &Kernel,
    scope: &str,
    session_id: Option<&str>,
    installed_tools: &[InstalledToolDefinition],
    installed_integrations: &[InstalledIntegrationDefinition],
) -> Result<(), KernelError> {
    kernel
        .process_direct(Transaction::session_start(kernel.agent_id))
        .await?;

    let payload = RuntimeCapabilityInstall {
        system_kind: SystemKind::CapabilityInstall,
        scope: scope.to_string(),
        session_id: session_id.map(str::to_string),
        installed_integrations: installed_integrations.to_vec(),
        installed_tools: installed_tools
            .iter()
            .map(InstalledToolCapability::from)
            .collect(),
    };
    let payload_bytes = serde_json::to_vec(&payload)
        .map_err(|e| KernelError::Serialization(format!("serialize capability install: {e}")))?;
    let tx = Transaction::new_chained(
        kernel.agent_id,
        TransactionType::System,
        payload_bytes,
        None,
    );
    kernel.process_direct(tx).await?;
    Ok(())
}
