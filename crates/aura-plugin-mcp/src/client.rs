//! Newline-delimited JSON-RPC 2.0 client over stdio.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Each [`McpClient`] owns one child process. Drop kills + waits.
//! - The request id sequence is monotone per client (1..) and the
//!   server must echo it back as the response `"id"`.
//! - On child exit / pipe error every subsequent request returns
//!   [`McpError::Disconnected`]. Phase 4c does not auto-restart; the
//!   manager owns that policy in Phase 8+.
//! - The child env is explicitly cleared and re-populated from
//!   [`crate::ServerConfig::env`]. No parent-env inheritance.
//!
//! ## Phase scope
//!
//! Phase 4c ships stdio only. WebSocket / HTTP transports land in a
//! later phase. The [`McpClient::request`] surface is shaped to
//! accommodate alternative transports without a public-API churn
//! (`method` + `params` + JSON result mirror the JSON-RPC contract).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use serde_json::Value;

use crate::error::McpError;

/// Stdio JSON-RPC client. One client owns one child process.
pub struct McpClient {
    child: Child,
    writer: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn a child process and wire its stdio pipes.
    ///
    /// The child env is cleared first and re-populated from `env`
    /// (no parent-env inheritance). The caller is responsible for
    /// constructing `env` with whatever the server needs — see the
    /// module-level docs for the invariant rationale.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Io`] for spawn failures and
    /// [`McpError::Disconnected`] if the child's stdio handles
    /// cannot be claimed (in practice this only happens when
    /// `Stdio::piped()` is ignored by the OS — never observed in
    /// practice but kept as an explicit failure mode rather than a
    /// panic).
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args).env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let writer = child.stdin.take().ok_or(McpError::Disconnected)?;
        let reader = BufReader::new(child.stdout.take().ok_or(McpError::Disconnected)?);
        Ok(Self {
            child,
            writer,
            reader,
            next_id: Mutex::new(1),
        })
    }

    /// Send a single JSON-RPC request and block on the response.
    ///
    /// # Errors
    ///
    /// - [`McpError::Io`] on pipe write / read failures.
    /// - [`McpError::Disconnected`] when the server closes its
    ///   stdout without sending a response.
    /// - [`McpError::InvalidResponse`] when the response is not
    ///   valid JSON.
    /// - [`McpError::ServerError`] when the response carries an
    ///   `"error"` object.
    pub fn request(&mut self, method: &str, params: &Value) -> Result<Value, McpError> {
        let id = {
            let mut guard = self
                .next_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let v = *guard;
            *guard += 1;
            v
        };
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| McpError::InvalidResponse(format!("serialize request: {e}")))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(McpError::Disconnected);
        }
        let resp: Value = serde_json::from_str(&buf)
            .map_err(|e| McpError::InvalidResponse(format!("parse response: {e}")))?;
        if let Some(err) = resp.get("error") {
            let code = err
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(-1);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Err(McpError::ServerError { code, message });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Best-effort kill of the underlying child process. Called
    /// automatically on `Drop`; exposed publicly for callers that
    /// want deterministic teardown before the client is dropped.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.kill();
    }
}
