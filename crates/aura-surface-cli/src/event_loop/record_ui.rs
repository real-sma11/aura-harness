use crate::record_loader::extract_tool_info;
use aura_core::{AgentId, EffectStatus, RecordEntry, Transaction, TransactionType};
use aura_terminal::{
    events::{RecordStatus, RecordSummary},
    UiCommand,
};
use tokio::sync::mpsc;

/// Send a record summary to the UI (matching the stored format).
pub(crate) async fn send_record_to_ui(
    commands: &mpsc::Sender<UiCommand>,
    seq: u64,
    tx: &Transaction,
    entry: &RecordEntry,
) {
    let (kind, sndr) = crate::record_loader::tx_type_label(tx.tx_type);
    let (tx_kind, sender) = (kind.to_string(), sndr.to_string());

    let message = String::from_utf8_lossy(&tx.payload).to_string();
    let message = if message.len() > 200 {
        format!("{}...", &message[..197])
    } else {
        message
    };

    let effect_count = entry.effects.len();
    let ok_count = entry
        .effects
        .iter()
        .filter(|e| matches!(e.status, EffectStatus::Committed))
        .count();
    let pending_count = entry
        .effects
        .iter()
        .filter(|e| matches!(e.status, EffectStatus::Pending))
        .count();
    let err_count = effect_count - ok_count - pending_count;

    let effect_status = if effect_count == 0 {
        "-".to_string()
    } else if err_count == 0 {
        format!("{ok_count} ok")
    } else {
        format!("{ok_count} ok, {err_count} err")
    };

    let status = if err_count > 0 {
        RecordStatus::Error
    } else if pending_count > 0 {
        RecordStatus::Pending
    } else {
        RecordStatus::Ok
    };

    let error_details: String = entry
        .effects
        .iter()
        .filter(|e| matches!(e.status, EffectStatus::Failed))
        .filter_map(|e| String::from_utf8(e.payload.to_vec()).ok())
        .collect::<Vec<_>>()
        .join("; ");

    let info = extract_tool_info(tx);

    let full_hash = entry.context_hash.as_hex();
    let hash_suffix = full_hash[full_hash.len() - 4..].to_string();

    let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();

    let record_summary = RecordSummary {
        seq,
        timestamp,
        full_hash,
        hash_suffix,
        tx_kind,
        sender,
        message,
        action_count: entry.actions.len(),
        effect_status,
        status,
        info,
        error_details,
        tx_id: hex::encode(tx.hash.as_bytes()),
        agent_id: hex::encode(tx.agent_id.as_bytes()),
        ts_ms: tx.ts_ms,
    };

    let _ = commands.send(UiCommand::NewRecord(record_summary)).await;
}

/// Create a response transaction for the assistant's message.
pub(crate) fn create_response_transaction(agent_id: AgentId, response_text: &str) -> Transaction {
    Transaction::new_chained(
        agent_id,
        TransactionType::AgentMsg,
        response_text.as_bytes().to_vec(),
        None,
    )
}
