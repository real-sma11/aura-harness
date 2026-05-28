//! Phase 9 — insta snapshot test fixing the documented
//! [`AgentMode`] resolution priority order.
//!
//! Each case below constructs all four inputs (CLI, TUI, SDK,
//! daemon) and asserts the resolved mode matches the expected
//! priority winner. The pinned snapshot file is the canonical
//! reference; changing the priority semantics requires updating
//! the snapshot.

use aura_core_modes::AgentMode;
use aura_fleet_daemon::{resolve_session_mode, AgentModeInputs};
use serde::Serialize;

/// One row of the priority table. Serialised into the snapshot.
#[derive(Debug, Serialize)]
struct Row {
    case: &'static str,
    cli: Option<&'static str>,
    tui: Option<&'static str>,
    sdk: Option<&'static str>,
    daemon: Option<&'static str>,
    resolved: &'static str,
}

fn name(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Agent => "agent",
        AgentMode::Plan => "plan",
        AgentMode::Ask => "ask",
        AgentMode::Debug => "debug",
    }
}

fn maybe(m: Option<AgentMode>) -> Option<&'static str> {
    m.map(name)
}

fn row(
    case: &'static str,
    cli: Option<AgentMode>,
    tui: Option<AgentMode>,
    sdk: Option<AgentMode>,
    daemon: Option<AgentMode>,
) -> Row {
    let inputs = AgentModeInputs {
        cli_flag: cli,
        tui_slash: tui,
        sdk_field: sdk,
        daemon_default: daemon,
    };
    let resolved = resolve_session_mode(inputs);
    Row {
        case,
        cli: maybe(cli),
        tui: maybe(tui),
        sdk: maybe(sdk),
        daemon: maybe(daemon),
        resolved: name(resolved),
    }
}

#[test]
fn priority_order_snapshot() {
    let cases = vec![
        row("01_all_unset__fallback", None, None, None, None),
        row(
            "02_daemon_only_plan",
            None,
            None,
            None,
            Some(AgentMode::Plan),
        ),
        row(
            "03_sdk_wins_over_daemon",
            None,
            None,
            Some(AgentMode::Ask),
            Some(AgentMode::Plan),
        ),
        row(
            "04_tui_wins_over_sdk_and_daemon",
            None,
            Some(AgentMode::Debug),
            Some(AgentMode::Ask),
            Some(AgentMode::Plan),
        ),
        row(
            "05_cli_wins_over_all_others",
            Some(AgentMode::Plan),
            Some(AgentMode::Debug),
            Some(AgentMode::Ask),
            Some(AgentMode::Agent),
        ),
        row(
            "06_cli_only_agent",
            Some(AgentMode::Agent),
            None,
            None,
            None,
        ),
        row(
            "07_tui_only_debug",
            None,
            Some(AgentMode::Debug),
            None,
            None,
        ),
        row("08_sdk_only_ask", None, None, Some(AgentMode::Ask), None),
        row(
            "09_daemon_wins_when_higher_rungs_none",
            None,
            None,
            None,
            Some(AgentMode::Debug),
        ),
        row(
            "10_cli_plan_overrides_daemon_ask",
            Some(AgentMode::Plan),
            None,
            None,
            Some(AgentMode::Ask),
        ),
    ];

    insta::assert_json_snapshot!("priority_order", cases);
}

#[test]
fn fallback_is_agent_when_all_inputs_none() {
    let resolved = resolve_session_mode(AgentModeInputs::default());
    assert_eq!(resolved, AgentMode::Agent);
}

#[test]
fn cli_flag_strictly_wins_over_every_other_rung() {
    for cli in [
        AgentMode::Agent,
        AgentMode::Plan,
        AgentMode::Ask,
        AgentMode::Debug,
    ] {
        let resolved = resolve_session_mode(AgentModeInputs {
            cli_flag: Some(cli),
            tui_slash: Some(AgentMode::Plan),
            sdk_field: Some(AgentMode::Ask),
            daemon_default: Some(AgentMode::Debug),
        });
        assert_eq!(resolved, cli, "cli flag must always win");
    }
}
