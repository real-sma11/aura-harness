//! Tool call envelope and credential-redaction helpers used by `Debug`.

use serde::{Deserialize, Serialize};

/// A tool call request.
///
/// # Redacted `Debug`
///
/// The `Debug` impl is **hand-written** (not derived) so accidental
/// `{:?}` formatting of a `ToolCall` — including via `#[instrument]`
/// spans that don't skip the argument — cannot leak credentials.
/// Any key in [`SENSITIVE_ARG_KEYS`] is rendered as `"***"` in place
/// of its real value, at arbitrary nesting depth. `Serialize` is
/// unchanged: on-wire/JSON shape must still round-trip losslessly
/// for tool dispatch.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name (e.g., `list_files`, `read_file`, `run_command`)
    pub tool: String,
    /// Tool arguments (versioned JSON)
    pub args: serde_json::Value,
}

/// Argument keys whose values must never appear in logs / `Debug`
/// output. Matched case-insensitively against the final path segment
/// (the object key), at any depth inside `ToolCall.args`.
///
/// Keep this list narrow: only add keys that are *definitionally*
/// credentials. Things like `message` or `remote_url` are not secret
/// and need to remain visible for debugging push failures.
const SENSITIVE_ARG_KEYS: &[&str] = &[
    "jwt",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "authorization",
    "api_key",
    "apikey",
    "password",
    "passwd",
    "secret",
    "client_secret",
    "private_key",
    "session_token",
];

fn redact_sensitive_args(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if SENSITIVE_ARG_KEYS
                    .iter()
                    .any(|needle| k.eq_ignore_ascii_case(needle))
                {
                    out.insert(k.clone(), serde_json::Value::String("***".to_string()));
                } else {
                    out.insert(k.clone(), redact_sensitive_args(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive_args).collect())
        }
        other => other.clone(),
    }
}

impl std::fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall")
            .field("tool", &self.tool)
            .field("args", &redact_sensitive_args(&self.args))
            .finish()
    }
}

impl ToolCall {
    /// Create a new tool call.
    #[must_use]
    pub fn new(tool: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            tool: tool.into(),
            args,
        }
    }

    /// Create a `list_files` tool call.
    #[must_use]
    pub fn fs_ls(path: impl Into<String>) -> Self {
        Self::new("list_files", serde_json::json!({ "path": path.into() }))
    }

    /// Create a `read_file` tool call.
    #[must_use]
    pub fn fs_read(path: impl Into<String>, max_bytes: Option<usize>) -> Self {
        let mut args = serde_json::json!({ "path": path.into() });
        if let Some(max) = max_bytes {
            args["max_bytes"] = serde_json::json!(max);
        }
        Self::new("read_file", args)
    }

    /// Create a `stat_file` tool call.
    #[must_use]
    pub fn fs_stat(path: impl Into<String>) -> Self {
        Self::new("stat_file", serde_json::json!({ "path": path.into() }))
    }
}

#[cfg(test)]
mod tool_call_debug_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_redacts_jwt_in_top_level_args() {
        // Real-world shape: the `git_commit_push` tool is called with
        // a `jwt` field that must NEVER appear in logs.
        let call = ToolCall::new(
            "git_commit_push",
            json!({
                "branch": "main",
                "message": "task: completed",
                "remote_url": "https://orbit.example.com/proj.git",
                "jwt": "eyJhbGciOiJIUzI1NiIs.SECRETPAYLOAD.SECRETSIG",
            }),
        );
        let rendered = format!("{call:?}");
        assert!(
            !rendered.contains("SECRETPAYLOAD"),
            "raw JWT leaked: {rendered}"
        );
        assert!(
            !rendered.contains("eyJhbGciOi"),
            "JWT prefix leaked: {rendered}"
        );
        assert!(
            rendered.contains("\"***\""),
            "no redaction marker: {rendered}"
        );
        // Non-sensitive fields must remain visible for debugging.
        assert!(rendered.contains("main"), "branch missing: {rendered}");
        assert!(
            rendered.contains("https://orbit.example.com/proj.git"),
            "remote_url missing: {rendered}"
        );
    }

    #[test]
    fn debug_redacts_case_insensitive_and_nested() {
        let call = ToolCall::new(
            "some_tool",
            json!({
                "outer": {
                    "Authorization": "Bearer SHOULDNEVERAPPEAR",
                    "nested": {
                        "API_KEY": "KEYSHOULDNEVERAPPEAR",
                        "keep_me": "visible"
                    }
                },
                "items": [
                    { "password": "PWDSHOULDNEVERAPPEAR" },
                    { "name": "ok" }
                ]
            }),
        );
        let rendered = format!("{call:?}");
        assert!(
            !rendered.contains("SHOULDNEVERAPPEAR"),
            "nested bearer leaked: {rendered}"
        );
        assert!(
            !rendered.contains("KEYSHOULDNEVERAPPEAR"),
            "nested api key leaked: {rendered}"
        );
        assert!(
            !rendered.contains("PWDSHOULDNEVERAPPEAR"),
            "array-nested password leaked: {rendered}"
        );
        assert!(
            rendered.contains("visible"),
            "non-sensitive value dropped: {rendered}"
        );
        assert!(
            rendered.contains("\"ok\""),
            "non-sensitive array field dropped: {rendered}"
        );
    }

    #[test]
    fn debug_preserves_serialize_roundtrip() {
        // Serialize must NOT redact — tool dispatch over the wire
        // needs the real value. Redaction is a Debug-only concern.
        let call = ToolCall::new("t", json!({ "jwt": "real-token-here" }));
        let wire = serde_json::to_string(&call).expect("serialize");
        assert!(
            wire.contains("real-token-here"),
            "serialize was redacted: {wire}"
        );
        let round: ToolCall = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(round, call);
    }
}
