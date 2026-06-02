//! Computer-use tool.
//!
//! [`ComputerTool`] is the harness execution side of Anthropic's
//! computer-use capability. The model emits `computer` tool-use blocks
//! in Anthropic's computer-use action shape (`action` + `coordinate` +
//! `text` + scroll/drag fields); this tool translates each action into
//! the desktop executor's flat HTTP body and POSTs it to
//! `{executor_url}/api/computer/action`. The executor performs the real
//! OS side effect and replies with `{ ok, image_base64?, width?,
//! height?, error? }`. A successful screenshot is carried back to the
//! model as an image [`ToolResult`] (media type `image/png`).
//!
//! ## Boundary + safety invariants
//!
//! - **Side effects are behind the executor boundary.** This tool never
//!   touches the OS directly; it only forwards validated/clamped
//!   actions over HTTP.
//! - **Bounded timeouts.** The HTTP client carries request + connect
//!   timeouts (mirrors [`crate::http_tool`]) so a hung executor cannot
//!   stall an agent turn.
//! - **No payload logging.** The base64 screenshot is never logged;
//!   only the action name and reported dimensions are.
//! - **Coordinate clamping.** Model-supplied coordinates are clamped to
//!   the advertised virtual display so an out-of-range click cannot be
//!   forwarded verbatim.

use std::time::Duration;

use async_trait::async_trait;
use aura_core_types::{Capability, ToolDefinition, ToolResult};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use crate::error::ToolError;
use crate::tool::{Tool, ToolContext};

/// Tool name exposed to the model (must match the Anthropic
/// `computer_20250124` tool name and the harness catalog entry).
pub const COMPUTER_TOOL_NAME: &str = "computer";

/// Request-total timeout for an executor call. Generous enough to cover
/// a server-side `wait` action plus screenshot capture, but still
/// bounded so a wedged executor cannot hang the turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Connect-phase timeout: fast-fail an unreachable executor.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the executor response body held in memory. A 1280x800 PNG
/// base64 is well under this; the cap guards against a pathological
/// payload blowing up memory.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Virtual display geometry. Mirrors the `display_width_px` /
/// `display_height_px` advertised to Anthropic in the request builder so
/// model coordinates land on the same grid the executor renders.
const DISPLAY_WIDTH_PX: i64 = 1280;
const DISPLAY_HEIGHT_PX: i64 = 800;

/// Executor action endpoint (appended to the configured base URL).
const ACTION_PATH: &str = "/api/computer/action";

/// Media type stamped on the returned screenshot.
const SCREENSHOT_MEDIA_TYPE: &str = "image/png";

/// Default scroll magnitude when the model omits `scroll_amount`.
const DEFAULT_SCROLL_AMOUNT: i64 = 3;

/// Upper bound on a `wait` action's duration (ms), so a model can't ask
/// the executor to block for an unbounded period.
const MAX_WAIT_MS: f64 = 60_000.0;

/// Actions the desktop executor understands.
const SUPPORTED_ACTIONS: &[&str] = &[
    "screenshot",
    "mouse_move",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "left_click_drag",
    "type",
    "key",
    "scroll",
    "wait",
];

/// Computer-use tool: forwards actions to a desktop executor.
pub struct ComputerTool {
    /// Base URL of the desktop executor (no trailing slash).
    executor_url: String,
    /// Bounded HTTP client shared across calls.
    client: Client,
}

/// Desktop executor response envelope.
#[derive(Debug, Deserialize)]
struct ExecutorResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    image_base64: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    error: Option<String>,
}

impl ComputerTool {
    /// Build a computer-use tool bound to `executor_url`.
    ///
    /// The HTTP client carries request + connect timeouts. On the rare
    /// event that the TLS backend fails to initialize we log a warning
    /// and fall back to a naive client (losing timeouts is preferable to
    /// failing tool registration).
    #[must_use]
    pub fn new(executor_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                warn!(
                    error = %e,
                    "failed to build timed computer-use HTTP client; falling back to default"
                );
                Client::new()
            });
        Self {
            executor_url: executor_url.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Model-facing input schema describing the executor action set.
    ///
    /// When the run is in computer-use mode the request builder replaces
    /// this with Anthropic's built-in `computer_20250124` tool entry, so
    /// this schema is the fallback / documentation surface (and what
    /// non-computer-use contexts, e.g. token estimation, read).
    #[must_use]
    pub fn input_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": SUPPORTED_ACTIONS,
                    "description": "The computer action to perform."
                },
                "coordinate": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[x, y] target pixel for mouse actions."
                },
                "start_coordinate": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[x, y] start pixel for left_click_drag."
                },
                "text": {
                    "type": "string",
                    "description": "Text to type, or key combo for the key action."
                },
                "scroll_direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction for the scroll action."
                },
                "scroll_amount": {
                    "type": "integer",
                    "description": "Number of scroll clicks for the scroll action."
                },
                "duration": {
                    "type": "number",
                    "description": "Seconds to pause for the wait action."
                }
            },
            "required": ["action"]
        })
    }
}

/// Canonical model-facing [`ToolDefinition`] for the computer tool.
///
/// Shared by the catalog (capability-gated visibility entry) and the
/// live [`ComputerTool`] impl so both advertise the identical schema.
#[must_use]
pub fn computer_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        COMPUTER_TOOL_NAME,
        "Control the computer: move/click the mouse, type text, press keys, scroll, \
         wait, and capture screenshots. Actions are forwarded to the desktop executor \
         and return a screenshot of the resulting screen.",
        ComputerTool::input_schema(),
    )
}

/// Clamp a single coordinate axis into `[0, max)`.
fn clamp_axis(value: i64, max: i64) -> i64 {
    value.clamp(0, max - 1)
}

/// Convert a pre-clamped, rounded millisecond value to `u64`.
///
/// The caller guarantees `ms` is finite, `>= 0.0`, and `<= MAX_WAIT_MS`
/// (60000), so the truncating cast cannot lose magnitude or sign — the
/// lint is scoped + documented per that invariant.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ms_to_u64(ms: f64) -> u64 {
    ms as u64
}

/// Extract a clamped `[x, y]` pair from `args[key]` if present + valid.
fn coordinate(args: &Value, key: &str) -> Option<(i64, i64)> {
    let arr = args.get(key)?.as_array()?;
    let x = arr.first()?.as_i64()?;
    let y = arr.get(1)?.as_i64()?;
    Some((
        clamp_axis(x, DISPLAY_WIDTH_PX),
        clamp_axis(y, DISPLAY_HEIGHT_PX),
    ))
}

/// Translate Anthropic `scroll_direction` + `scroll_amount` into the
/// executor's `(dx, dy)` delta.
fn scroll_delta(args: &Value) -> (i64, i64) {
    let amount = args
        .get("scroll_amount")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_SCROLL_AMOUNT)
        .max(0);
    match args.get("scroll_direction").and_then(Value::as_str) {
        Some("up") => (0, -amount),
        Some("left") => (-amount, 0),
        Some("right") => (amount, 0),
        // Default + explicit "down" scroll downward.
        _ => (0, amount),
    }
}

/// Validate the action and build the flat executor request body from
/// the model-supplied Anthropic computer-use input.
fn build_executor_body(args: &Value) -> Result<Value, ToolError> {
    let action = args.get("action").and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidArguments("computer tool requires an 'action' string".to_string())
    })?;
    if !SUPPORTED_ACTIONS.contains(&action) {
        return Err(ToolError::InvalidArguments(format!(
            "unsupported computer action '{action}'"
        )));
    }

    let mut body: Map<String, Value> = Map::new();
    body.insert("action".to_string(), json!(action));

    if let Some((x, y)) = coordinate(args, "coordinate") {
        body.insert("x".to_string(), json!(x));
        body.insert("y".to_string(), json!(y));
    }

    if action == "left_click_drag" {
        if let (Some((sx, sy)), Some((x, y))) = (
            coordinate(args, "start_coordinate"),
            coordinate(args, "coordinate"),
        ) {
            body.insert("dx".to_string(), json!(x - sx));
            body.insert("dy".to_string(), json!(y - sy));
        }
    }

    if action == "scroll" {
        let (dx, dy) = scroll_delta(args);
        body.insert("dx".to_string(), json!(dx));
        body.insert("dy".to_string(), json!(dy));
    }

    if let Some(text) = args.get("text").and_then(Value::as_str) {
        // Anthropic carries the key combo for the `key` action in `text`;
        // the executor expects it in its dedicated `key` field.
        let field = if action == "key" { "key" } else { "text" };
        body.insert(field.to_string(), json!(text));
    }

    if action == "wait" {
        if let Some(seconds) = args.get("duration").and_then(Value::as_f64) {
            // Clamp to `[0, MAX_WAIT_MS]` then round to whole ms. The
            // value is provably in-range for `i64`, so serialize it as a
            // JSON number without an `as` cast (avoids a truncation lint
            // and keeps the bound explicit).
            let ms = (seconds * 1000.0).clamp(0.0, MAX_WAIT_MS).round();
            body.insert(
                "duration_ms".to_string(),
                Value::Number(serde_json::Number::from(ms_to_u64(ms))),
            );
        }
    }

    Ok(Value::Object(body))
}

/// Render the reported screen dimensions for log/result text.
fn dims_label(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(w), Some(h)) => format!("{w}x{h}"),
        _ => "unknown".to_string(),
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        COMPUTER_TOOL_NAME
    }

    fn definition(&self) -> ToolDefinition {
        computer_tool_definition()
    }

    fn required_capabilities(&self) -> Vec<Capability> {
        vec![Capability::ComputerUse]
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let body = build_executor_body(&args)?;
        let action = body
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let url = format!("{}{ACTION_PATH}", self.executor_url);

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ToolError::CommandFailed(format!("computer executor request failed: {e}"))
            })?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| {
            ToolError::CommandFailed(format!("read computer executor response: {e}"))
        })?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ToolError::SizeLimitExceeded {
                actual: bytes.len(),
                limit: MAX_RESPONSE_BYTES,
            });
        }

        if !status.is_success() {
            let preview: String = String::from_utf8_lossy(&bytes).chars().take(512).collect();
            warn!(action = %action, status = status.as_u16(), "computer executor non-2xx");
            return Ok(ToolResult::failure(
                COMPUTER_TOOL_NAME,
                format!(
                    "computer executor returned HTTP {}: {preview}",
                    status.as_u16()
                ),
            ));
        }

        let parsed: ExecutorResponse = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    COMPUTER_TOOL_NAME,
                    format!("could not parse computer executor response: {e}"),
                ));
            }
        };

        if !parsed.ok {
            let reason = parsed
                .error
                .unwrap_or_else(|| "computer executor reported failure".to_string());
            warn!(action = %action, "computer action failed: {reason}");
            return Ok(ToolResult::failure(COMPUTER_TOOL_NAME, reason));
        }

        let dims = dims_label(parsed.width, parsed.height);
        // NEVER log the base64 payload — only the action + dims.
        debug!(
            action = %action,
            screen = %dims,
            has_image = parsed.image_base64.is_some(),
            "computer action executed"
        );

        let content = format!("computer action '{action}' ok (screen {dims})");
        let mut result = ToolResult::success(COMPUTER_TOOL_NAME, content.into_bytes());
        if let Some(base64) = parsed.image_base64 {
            result = result.with_image(base64, SCREENSHOT_MEDIA_TYPE);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;

    use crate::sandbox::Sandbox;
    use crate::ToolConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn ctx() -> ToolContext {
        let dir = std::env::temp_dir();
        ToolContext::new(Sandbox::new(&dir).unwrap(), ToolConfig::default())
    }

    /// Minimal single-connection HTTP/1.1 mock returning a fixed JSON body.
    async fn start_mock(
        status: u16,
        body: &'static str,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8 * 1024];
            let _ = sock.read(&mut buf).await.unwrap_or(0);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (addr, handle)
    }

    #[test]
    fn build_body_clamps_coordinates() {
        let body = build_executor_body(&json!({"action": "left_click", "coordinate": [5000, -10]}))
            .expect("valid action");
        assert_eq!(body["x"], json!(DISPLAY_WIDTH_PX - 1));
        assert_eq!(body["y"], json!(0));
        assert_eq!(body["action"], json!("left_click"));
    }

    #[test]
    fn build_body_maps_key_text_to_key_field() {
        let body =
            build_executor_body(&json!({"action": "key", "text": "ctrl+s"})).expect("valid action");
        assert_eq!(body["key"], json!("ctrl+s"));
        assert!(body.get("text").is_none());
    }

    #[test]
    fn build_body_maps_type_text_to_text_field() {
        let body =
            build_executor_body(&json!({"action": "type", "text": "hello"})).expect("valid action");
        assert_eq!(body["text"], json!("hello"));
    }

    #[test]
    fn build_body_translates_scroll_direction() {
        let body = build_executor_body(
            &json!({"action": "scroll", "scroll_direction": "up", "scroll_amount": 4}),
        )
        .expect("valid action");
        assert_eq!(body["dx"], json!(0));
        assert_eq!(body["dy"], json!(-4));
    }

    #[test]
    fn build_body_rejects_unknown_action() {
        let err = build_executor_body(&json!({"action": "teleport"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn execute_screenshot_returns_image_result() {
        let (addr, _h) = start_mock(
            200,
            r#"{"ok":true,"image_base64":"aGVsbG8=","width":1280,"height":800}"#,
        )
        .await;
        let tool = ComputerTool::new(format!("http://{addr}"));
        let result = tool
            .execute(&ctx(), json!({"action": "screenshot"}))
            .await
            .expect("execute ok");
        assert!(result.ok);
        let image = result.image.expect("image attached");
        assert_eq!(image.base64, "aGVsbG8=");
        assert_eq!(image.media_type, SCREENSHOT_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn execute_executor_error_maps_to_failure() {
        let (addr, _h) = start_mock(200, r#"{"ok":false,"error":"display not found"}"#).await;
        let tool = ComputerTool::new(format!("http://{addr}"));
        let result = tool
            .execute(&ctx(), json!({"action": "screenshot"}))
            .await
            .expect("execute ok");
        assert!(!result.ok);
        assert!(result.image.is_none());
        assert_eq!(String::from_utf8_lossy(&result.stderr), "display not found");
    }

    #[tokio::test]
    async fn execute_non_2xx_maps_to_failure() {
        let (addr, _h) = start_mock(500, r#"{"error":"boom"}"#).await;
        let tool = ComputerTool::new(format!("http://{addr}"));
        let result = tool
            .execute(&ctx(), json!({"action": "screenshot"}))
            .await
            .expect("execute ok");
        assert!(!result.ok);
    }

    #[test]
    fn required_capability_is_computer_use() {
        let tool = ComputerTool::new("http://127.0.0.1:1");
        assert_eq!(tool.required_capabilities(), vec![Capability::ComputerUse]);
        assert_eq!(tool.name(), "computer");
    }
}
