//! Domain types for the Aura system.
//!
//! Includes Transaction, Action, Effect, `RecordEntry`, and related types.

mod action;
mod context_hash;
mod effect;
mod identity;
mod process;
mod proposal;
mod reasoner_types;
mod record;
mod status;
mod subagent;
mod tool;
mod tool_permissions;
mod transaction;

pub use action::{Action, ActionKind};
pub use context_hash::ContextHash;
pub use effect::{Effect, EffectKind, EffectStatus};
pub use identity::Identity;
pub use process::{ActionResultPayload, ProcessPending};
pub use proposal::{Decision, Proposal, ProposalSet, RejectedProposal, Trace};
pub use reasoner_types::{CacheControl, ToolDefinition, ToolResultContent};
pub use record::RecordEntry;
pub use status::AgentStatus;
pub use subagent::{
    SubagentBudget, SubagentDispatchRequest, SubagentExit, SubagentKindSpec, SubagentResult,
    DEFAULT_SUBAGENT_TIMEOUT_MS, MAX_TURNS,
};
#[allow(deprecated)]
pub use tool::ToolDecision;
pub use tool::{
    installed_integrations_satisfy, integration_match, InstalledIntegrationDefinition,
    InstalledToolCapability, InstalledToolDefinition, InstalledToolIntegrationRequirement,
    InstalledToolRuntimeAuth, InstalledToolRuntimeExecution, InstalledToolRuntimeIntegration,
    InstalledToolRuntimeProviderExecution, LineDiff, RuntimeCapabilityInstall, ToolAuth, ToolCall,
    ToolCallContext, ToolExecution, ToolGateVerdict, ToolProposal, ToolResult, ToolResultKind,
};
pub use tool_permissions::{
    is_effectively_full_access, resolve_effective_permission, AgentToolPermissions, ToolState,
    UserDefaultMode, UserToolDefaults,
};
pub use transaction::{SystemKind, Transaction, TransactionType};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ActionId, AgentId, ProcessId};
    use proptest::prelude::*;

    fn arb_agent_id() -> impl Strategy<Value = AgentId> {
        any::<[u8; 32]>().prop_map(AgentId::new)
    }

    fn arb_action_id() -> impl Strategy<Value = ActionId> {
        any::<[u8; 16]>().prop_map(ActionId::new)
    }

    fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
        prop_oneof![
            Just(ActionKind::Reason),
            Just(ActionKind::Memorize),
            Just(ActionKind::Decide),
            Just(ActionKind::Delegate),
        ]
    }

    fn arb_effect_kind() -> impl Strategy<Value = EffectKind> {
        prop_oneof![
            Just(EffectKind::Proposal),
            Just(EffectKind::Artifact),
            Just(EffectKind::Belief),
            Just(EffectKind::Agreement),
        ]
    }

    fn arb_effect_status() -> impl Strategy<Value = EffectStatus> {
        prop_oneof![
            Just(EffectStatus::Committed),
            Just(EffectStatus::Pending),
            Just(EffectStatus::Failed),
        ]
    }

    fn arb_tx_type() -> impl Strategy<Value = TransactionType> {
        prop_oneof![
            Just(TransactionType::UserPrompt),
            Just(TransactionType::AgentMsg),
            Just(TransactionType::Trigger),
            Just(TransactionType::ActionResult),
            Just(TransactionType::System),
            Just(TransactionType::SessionStart),
            Just(TransactionType::ToolProposal),
            Just(TransactionType::ToolExecution),
            Just(TransactionType::ProcessComplete),
            Just(TransactionType::Reasoning),
        ]
    }

    proptest! {
        #[test]
        fn proptest_transaction_serde_roundtrip(
            agent_id in arb_agent_id(),
            tx_type in arb_tx_type(),
            payload in proptest::collection::vec(any::<u8>(), 0..256),
            ts_ms in any::<u64>(),
        ) {
            let hash = crate::ids::Hash::from_content(&payload);
            let tx = Transaction::new(hash, agent_id, ts_ms, tx_type, payload);
            let json = serde_json::to_string(&tx).unwrap();
            let parsed: Transaction = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(tx, parsed);
        }

        #[test]
        fn proptest_action_serde_roundtrip(
            action_id in arb_action_id(),
            kind in arb_action_kind(),
            payload in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let action = Action::new(action_id, kind, payload);
            let json = serde_json::to_string(&action).unwrap();
            let parsed: Action = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(action, parsed);
        }

        #[test]
        fn proptest_effect_serde_roundtrip(
            action_id in arb_action_id(),
            kind in arb_effect_kind(),
            status in arb_effect_status(),
            payload in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let effect = Effect::new(action_id, kind, status, payload);
            let json = serde_json::to_string(&effect).unwrap();
            let parsed: Effect = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(effect, parsed);
        }

        #[test]
        fn proptest_proposal_serde_roundtrip(
            kind in arb_action_kind(),
            payload in proptest::collection::vec(any::<u8>(), 0..128),
            has_rationale in any::<bool>(),
            rationale in "[a-zA-Z0-9 ]{0,64}",
        ) {
            let mut proposal = Proposal::new(kind, payload);
            if has_rationale {
                proposal = proposal.with_rationale(rationale);
            }
            let json = serde_json::to_string(&proposal).unwrap();
            let parsed: Proposal = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(proposal, parsed);
        }

        #[test]
        fn proptest_record_entry_serde_roundtrip(
            agent_id in arb_agent_id(),
            seq in 1..1000u64,
            context_hash in any::<[u8; 32]>(),
            payload in proptest::collection::vec(any::<u8>(), 1..128),
        ) {
            let hash = crate::ids::Hash::from_content(&payload);
            let tx = Transaction::new(hash, agent_id, 1000, TransactionType::UserPrompt, payload);
            let entry = RecordEntry::builder(seq, tx)
                .context_hash(context_hash)
                .proposals(ProposalSet::new())
                .decision(Decision::new())
                .actions(vec![])
                .effects(vec![])
                .build();
            let json = serde_json::to_string(&entry).unwrap();
            let parsed: RecordEntry = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(entry, parsed);
        }
    }

    #[test]
    fn transaction_roundtrip() {
        let tx = Transaction::user_prompt(AgentId::generate(), b"Hello, agent!".to_vec());
        let json = serde_json::to_string(&tx).unwrap();
        let parsed: Transaction = serde_json::from_str(&json).unwrap();
        assert_eq!(tx, parsed);
    }

    #[test]
    fn transaction_with_reference() {
        let agent_id = AgentId::generate();
        let orig_tx = Transaction::user_prompt(agent_id, b"start process".to_vec());
        let result_payload = ActionResultPayload::success(
            ActionId::generate(),
            ProcessId::generate(),
            Some(0),
            b"output".to_vec(),
            1000,
        );
        let callback_tx = Transaction::process_complete(
            agent_id,
            &result_payload,
            orig_tx.hash,
            Some(&orig_tx.hash),
        )
        .unwrap();

        assert_eq!(callback_tx.reference_tx_hash, Some(orig_tx.hash));
        assert_eq!(callback_tx.tx_type, TransactionType::ProcessComplete);

        let json = serde_json::to_string(&callback_tx).unwrap();
        let parsed: Transaction = serde_json::from_str(&json).unwrap();
        assert_eq!(callback_tx, parsed);
    }

    #[test]
    fn transaction_chaining() {
        let agent_id = AgentId::generate();

        let tx1 = Transaction::user_prompt(agent_id, b"first".to_vec());
        let tx2 = Transaction::user_prompt_chained(agent_id, b"second".to_vec(), &tx1.hash);

        let tx3 = Transaction::user_prompt(agent_id, b"second".to_vec());
        assert_ne!(tx2.hash, tx3.hash);

        let tx4 = Transaction::user_prompt_chained(agent_id, b"second".to_vec(), &tx1.hash);
        assert_eq!(tx2.hash, tx4.hash);
    }

    #[test]
    fn action_roundtrip() {
        let action = Action::new(
            ActionId::generate(),
            ActionKind::Delegate,
            b"tool payload".to_vec(),
        );
        let json = serde_json::to_string(&action).unwrap();
        let parsed: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(action, parsed);
    }

    #[test]
    fn effect_roundtrip() {
        let effect = Effect::committed_agreement(ActionId::generate(), b"result".to_vec());
        let json = serde_json::to_string(&effect).unwrap();
        let parsed: Effect = serde_json::from_str(&json).unwrap();
        assert_eq!(effect, parsed);
    }

    #[test]
    fn record_entry_roundtrip() {
        let tx = Transaction::user_prompt(AgentId::generate(), b"test".to_vec());
        let entry = RecordEntry::builder(1, tx)
            .context_hash([1u8; 32])
            .proposals(ProposalSet::new())
            .decision(Decision::new())
            .actions(vec![])
            .effects(vec![])
            .build();

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RecordEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn identity_creation() {
        let identity = Identity::new("0://TestAgent", "Test Agent");
        assert!(!identity.zns_id.is_empty());
        assert_eq!(identity.name, "Test Agent");
    }

    #[test]
    fn tool_call_roundtrip() {
        let tool_call = ToolCall::fs_read("src/main.rs", Some(1024));
        let json = serde_json::to_string(&tool_call).unwrap();
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(tool_call, parsed);
    }

    #[test]
    fn tool_execution_roundtrip_preserves_parent_chain_fields() {
        let agent_id = AgentId::generate();
        let execution = ToolExecution {
            tool_use_id: "use-1".into(),
            tool: "spawn_agent".into(),
            args: serde_json::json!({"name":"child"}),
            decision: ToolGateVerdict::Approved,
            reason: None,
            result: Some("ok".into()),
            is_error: false,
            parent_agent_id: agent_id,
            originating_user_id: "user-42".into(),
        };
        let json = serde_json::to_string(&execution).unwrap();
        assert!(json.contains("parent_agent_id"));
        assert!(json.contains("originating_user_id"));
        let parsed: ToolExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(execution, parsed);
    }

    #[test]
    fn tool_execution_roundtrip_requires_parent_chain_fields() {
        let agent_id = AgentId::generate();
        let execution = ToolExecution {
            tool_use_id: "use-2".into(),
            tool: "read_file".into(),
            args: serde_json::json!({"path":"a.txt"}),
            decision: ToolGateVerdict::Approved,
            reason: None,
            result: None,
            is_error: false,
            parent_agent_id: agent_id,
            originating_user_id: "user-0".into(),
        };
        let json = serde_json::to_string(&execution).unwrap();
        assert!(json.contains("parent_agent_id"));
        assert!(json.contains("originating_user_id"));
        let parsed: ToolExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(execution, parsed);
    }

    #[test]
    fn tool_result_roundtrip() {
        let result =
            ToolResult::success("read_file", b"file contents".to_vec()).with_metadata("size", "13");
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, parsed);
    }

    #[test]
    fn process_pending_roundtrip() {
        let pending = ProcessPending::new(ProcessId::generate(), "cargo build --release");
        let json = serde_json::to_string(&pending).unwrap();
        let parsed: ProcessPending = serde_json::from_str(&json).unwrap();
        assert_eq!(pending, parsed);
    }

    #[test]
    fn action_result_payload_success_roundtrip() {
        let payload = ActionResultPayload::success(
            ActionId::generate(),
            ProcessId::generate(),
            Some(0),
            b"build succeeded".to_vec(),
            5000,
        );
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ActionResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, parsed);
        assert!(payload.success);
    }

    #[test]
    fn action_result_payload_failure_roundtrip() {
        let payload = ActionResultPayload::failure(
            ActionId::generate(),
            ProcessId::generate(),
            Some(1),
            b"build failed".to_vec(),
            3000,
        );
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: ActionResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, parsed);
        assert!(!payload.success);
    }

    #[test]
    fn reasoning_transaction_type_roundtrip() {
        let json = serde_json::to_string(&TransactionType::Reasoning).unwrap();
        assert_eq!(json, "\"reasoning\"");
        let parsed: TransactionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TransactionType::Reasoning);
    }

    #[test]
    fn transaction_type_serialization() {
        let types = vec![
            TransactionType::UserPrompt,
            TransactionType::AgentMsg,
            TransactionType::Trigger,
            TransactionType::ActionResult,
            TransactionType::System,
            TransactionType::SessionStart,
            TransactionType::ToolProposal,
            TransactionType::ToolExecution,
            TransactionType::ProcessComplete,
            TransactionType::Reasoning,
        ];

        for tx_type in types {
            let json = serde_json::to_string(&tx_type).unwrap();
            let parsed: TransactionType = serde_json::from_str(&json).unwrap();
            assert_eq!(tx_type, parsed);
        }
    }
}
