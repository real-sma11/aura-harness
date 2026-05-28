//! Loading and displaying historical records from the store.

use aura_core::{AgentId, EffectStatus, Identity, Transaction, TransactionType};
use aura_store::{ReadStore, RocksStore};
use aura_terminal::{
    events::{AgentSummary, RecordStatus, RecordSummary},
    UiCommand,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

/// Load existing records from the store and send to UI.
pub fn load_existing_records(
    store: &Arc<RocksStore>,
    agent_id: AgentId,
    commands: &mpsc::Sender<UiCommand>,
) {
    let head_seq = match store.get_head_seq(agent_id) {
        Ok(seq) => seq,
        Err(e) => {
            warn!(error = %e, "Failed to get head sequence");
            let _ = commands.try_send(UiCommand::ShowWarning(format!(
                "Could not load record history: {e}"
            )));
            return;
        }
    };

    if head_seq == 0 {
        return;
    }

    let from_seq = head_seq.saturating_sub(99).max(1);
    let records = match store.scan_record(agent_id, from_seq, 100) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(from_seq, head_seq, error = %e, "Failed to load records");
            let _ = commands.try_send(UiCommand::ShowWarning(format!(
                "Could not load {head_seq} historical records: {e}"
            )));
            return;
        }
    };

    for entry in records {
        let (tx_kind, sender) = tx_type_label(entry.tx.tx_type);
        let (tx_kind, sender) = (tx_kind.to_string(), sender.to_string());

        let message = String::from_utf8_lossy(&entry.tx.payload).to_string();
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

        let info = extract_tool_info(&entry.tx);

        let full_hash = entry.context_hash.as_hex();
        let hash_suffix = full_hash[full_hash.len() - 4..].to_string();

        let timestamp = chrono::DateTime::from_timestamp_millis(entry.tx.ts_ms as i64)
            .map(|dt| dt.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "??:??:??".to_string());

        let record_summary = RecordSummary {
            seq: entry.seq,
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
            tx_id: hex::encode(entry.tx.hash.as_bytes()),
            agent_id: hex::encode(entry.tx.agent_id.as_bytes()),
            ts_ms: entry.tx.ts_ms,
        };

        let _ = commands.try_send(UiCommand::NewRecord(record_summary));
    }
}

/// Send initial agent info to the UI.
pub fn send_initial_agent(
    identity: &Identity,
    store: &Arc<RocksStore>,
    commands: &mpsc::Sender<UiCommand>,
) {
    let record_count = store.get_head_seq(identity.agent_id).unwrap_or(0);
    let last_active = chrono::Local::now().format("%H:%M:%S").to_string();

    let agent = AgentSummary {
        id: hex::encode(identity.agent_id.as_bytes()),
        name: identity.name.clone(),
        zns_id: identity.zns_id.clone(),
        is_active: true,
        record_count,
        last_active,
    };

    let _ = commands.try_send(UiCommand::SetAgents(vec![agent]));
    let _ = commands.try_send(UiCommand::SetActiveAgent(hex::encode(
        identity.agent_id.as_bytes(),
    )));
}

/// Map a transaction type to its display label and sender name.
pub fn tx_type_label(tx_type: TransactionType) -> (&'static str, &'static str) {
    match tx_type {
        TransactionType::UserPrompt => ("Prompt", "USER"),
        TransactionType::ActionResult => ("Action", "SYSTEM"),
        TransactionType::System => ("System", "SYSTEM"),
        TransactionType::AgentMsg => ("Response", "AURA"),
        TransactionType::Trigger => ("Trigger", "SYSTEM"),
        TransactionType::SessionStart => ("Session", "SYSTEM"),
        TransactionType::ToolProposal => ("Propose", "LLM"),
        TransactionType::ToolExecution => ("Execute", "KERNEL"),
        TransactionType::ProcessComplete => ("Complete", "SYSTEM"),
        TransactionType::Reasoning => ("Reasoning", "KERNEL"),
        TransactionType::SubagentSpawn => ("Spawn", "FLEET"),
    }
}

/// Extract tool name or other info from a transaction payload.
pub fn extract_tool_info(tx: &Transaction) -> String {
    match tx.tx_type {
        TransactionType::ToolProposal => {
            if let Ok(proposal) = serde_json::from_slice::<aura_core::ToolProposal>(&tx.payload) {
                if proposal.tool == "run_command" {
                    return extract_cmd_run_command(&proposal.args);
                }
                return proposal.tool;
            }
        }
        TransactionType::ToolExecution => {
            if let Ok(execution) = serde_json::from_slice::<aura_core::ToolExecution>(&tx.payload) {
                if execution.tool == "run_command" {
                    return extract_cmd_run_command(&execution.args);
                }
                return execution.tool;
            }
        }
        _ => {}
    }

    String::new()
}

/// Extract the command string from cmd_run tool arguments.
fn extract_cmd_run_command(args: &serde_json::Value) -> String {
    let program = args["program"].as_str().unwrap_or("");
    let cmd_args: Vec<&str> = args["args"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if cmd_args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", cmd_args.join(" "))
    }
}
