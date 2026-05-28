//! Phase 9 — golden CLI help text test.
//!
//! Pins the `--help` output of every documented `aura ...` subcommand
//! to an `insta` snapshot so the Phase 9 refactor cannot accidentally
//! reshape the CLI surface. The Phase 9 plan calls this out
//! explicitly: backward compatibility for the CLI is hard, every
//! existing `aura ...` invocation and every output line must continue
//! to produce identical text.
//!
//! ## Why a snapshot test and not a hard-coded string
//!
//! `clap`'s help rendering is platform-sensitive on a single
//! dimension: the program name in `Usage:` lines reflects the actual
//! binary name (`aura` on Linux, `aura.exe` on Windows). To keep the
//! snapshot portable, we sanitise `aura.exe ` → `aura ` before
//! recording. Every other line — flags, summary text, subcommand
//! listings, value enums — must match byte-identically.

use std::process::Command;

fn sanitise(raw: &str) -> String {
    // `clap` renders the binary name into `Usage:` lines (`Usage:
    // aura.exe ...` on Windows, `Usage: aura ...` on Linux). Strip
    // the `.exe` so the snapshot is platform-portable.
    raw.replace("aura.exe", "aura")
        // Also normalise trailing CRLF that PowerShell may inject on
        // Windows pipelines.
        .replace("\r\n", "\n")
}

fn run_help(args: &[&str]) -> String {
    let bin = env!("CARGO_BIN_EXE_aura");
    let mut cmd = Command::new(bin);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg("--help");
    let output = cmd.output().expect("invoke aura --help");
    assert!(
        output.status.success(),
        "aura {} --help exited non-zero: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("aura help is UTF-8");
    sanitise(&stdout)
}

#[test]
fn cli_golden_top_level_help() {
    let help = run_help(&[]);
    insta::assert_snapshot!("top_level", help);
}

#[test]
fn cli_golden_run_help() {
    let help = run_help(&["run"]);
    insta::assert_snapshot!("run", help);
}

#[test]
fn cli_golden_login_help() {
    let help = run_help(&["login"]);
    insta::assert_snapshot!("login", help);
}

#[test]
fn cli_golden_logout_help() {
    let help = run_help(&["logout"]);
    insta::assert_snapshot!("logout", help);
}

#[test]
fn cli_golden_whoami_help() {
    let help = run_help(&["whoami"]);
    insta::assert_snapshot!("whoami", help);
}

#[test]
fn cli_golden_migrate_help() {
    let help = run_help(&["migrate"]);
    insta::assert_snapshot!("migrate", help);
}

#[test]
fn cli_golden_plugins_help() {
    let help = run_help(&["plugins"]);
    insta::assert_snapshot!("plugins", help);
}

#[test]
fn cli_golden_plugins_install_help() {
    let help = run_help(&["plugins", "install"]);
    insta::assert_snapshot!("plugins_install", help);
}

#[test]
fn cli_golden_plugins_list_help() {
    let help = run_help(&["plugins", "list"]);
    insta::assert_snapshot!("plugins_list", help);
}

#[test]
fn cli_golden_plugins_enable_help() {
    let help = run_help(&["plugins", "enable"]);
    insta::assert_snapshot!("plugins_enable", help);
}

#[test]
fn cli_golden_plugins_disable_help() {
    let help = run_help(&["plugins", "disable"]);
    insta::assert_snapshot!("plugins_disable", help);
}

#[test]
fn cli_golden_agents_help() {
    let help = run_help(&["agents"]);
    insta::assert_snapshot!("agents", help);
}

#[test]
fn cli_golden_agents_inspect_help() {
    let help = run_help(&["agents", "inspect"]);
    insta::assert_snapshot!("agents_inspect", help);
}

#[test]
fn cli_golden_agents_reap_help() {
    let help = run_help(&["agents", "reap"]);
    insta::assert_snapshot!("agents_reap", help);
}
