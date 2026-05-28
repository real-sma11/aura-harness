//! Tiny stdio echo "MCP server" used exclusively by the
//! `tests/mcp_echo.rs` integration test.
//!
//! Reads newline-delimited JSON-RPC requests on stdin, extracts the
//! `id` field, and writes back `{"jsonrpc":"2.0","id":<id>,"result":<params>}`.
//! Exits cleanly when stdin closes.
//!
//! This is deliberately a separate binary (rather than an inline
//! Rust closure inside the test) so the integration test exercises a
//! real subprocess + stdio round-trip — the same code path the
//! Phase 8 manifest-driven MCP servers will take.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut line = String::new();
    let mut reader = stdin.lock();
    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else {
            return;
        };
        if n == 0 {
            // EOF — parent closed stdin; exit cleanly.
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "echoed": true,
                "params": params,
            },
        });
        if writeln!(out, "{resp}").is_err() {
            return;
        }
        if out.flush().is_err() {
            return;
        }
    }
}
