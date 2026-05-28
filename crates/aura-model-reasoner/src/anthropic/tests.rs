use super::api_types::{ApiContent, ApiImageSource, ApiMessage, ApiToolChoice};
use super::convert::{
    build_system_block, convert_messages_to_api, convert_tool_choice, convert_tools_to_api,
    dedupe_tool_results, resolve_output_config, resolve_thinking,
};
use super::{AnthropicConfig, AnthropicProvider, ApiError};
use crate::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelRequestKind, PromptCacheRetention,
    ReasonerError, Role, StopReason, StreamEvent, ThinkingConfig, ThinkingEffort, ToolChoice,
    ToolDefinition, ToolResultContent,
};
use futures_util::StreamExt;
use std::time::Duration;

#[test]
fn test_config_new() {
    let config = AnthropicConfig::new("claude-3-haiku");
    assert_eq!(config.default_model, "claude-3-haiku");
    assert_eq!(config.base_url, "https://aura-router.onrender.com");
    assert!(config.prompt_caching_enabled);
}

#[test]
fn test_convert_messages() {
    let messages = vec![Message::user("Hello"), Message::assistant("Hi there!")];

    let api_msgs = convert_messages_to_api(&messages, true);
    assert_eq!(api_msgs.len(), 2);
    assert_eq!(api_msgs[0].role, "user");
    assert_eq!(api_msgs[1].role, "assistant");
}

#[test]
fn test_convert_tools() {
    let tools = vec![ToolDefinition::new(
        "fs.read",
        "Read a file",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        }),
    )];

    let api_tools = convert_tools_to_api(&tools, true);
    assert_eq!(api_tools.len(), 1);
    assert_eq!(api_tools[0].name, "fs.read");
}

#[test]
fn test_convert_tool_choice() {
    assert!(matches!(
        convert_tool_choice(&ToolChoice::Auto, true),
        Some(ApiToolChoice::Auto {
            disable_parallel_tool_use: None
        })
    ));
    assert!(matches!(
        convert_tool_choice(&ToolChoice::Required, true),
        Some(ApiToolChoice::Any {
            disable_parallel_tool_use: None
        })
    ));
    assert!(convert_tool_choice(&ToolChoice::None, true).is_none());
}

// ---------------------------------------------------------------------------
// Phase 3 — parallel tool use wire shape
// ---------------------------------------------------------------------------

#[test]
fn parallel_tool_use_default_true_omits_disable_field() {
    // Default `parallel_tool_use: true` must NOT emit
    // `disable_parallel_tool_use`. Confirms the wire payload stays
    // byte-identical to the pre-Phase-3 `{"type": "auto"}` shape, so
    // the existing log-summary / golden-body tests keep passing and
    // we don't accidentally regress on the wire when nothing
    // changes.
    let choice = convert_tool_choice(&ToolChoice::Auto, true).expect("Auto -> Some");
    let json = serde_json::to_value(&choice).unwrap();
    assert_eq!(json, serde_json::json!({ "type": "auto" }));
}

#[test]
fn parallel_tool_use_false_emits_disable_parallel_tool_use_true() {
    let choice = convert_tool_choice(&ToolChoice::Auto, false).expect("Auto -> Some");
    let json = serde_json::to_value(&choice).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "type": "auto", "disable_parallel_tool_use": true })
    );
}

#[test]
fn parallel_tool_use_false_propagates_to_required_and_tool_variants() {
    let any_choice = convert_tool_choice(&ToolChoice::Required, false).expect("Required -> Some");
    let any_json = serde_json::to_value(&any_choice).unwrap();
    assert_eq!(
        any_json,
        serde_json::json!({ "type": "any", "disable_parallel_tool_use": true })
    );

    let tool_choice = convert_tool_choice(
        &ToolChoice::Tool {
            name: "read_file".to_string(),
        },
        false,
    )
    .expect("Tool -> Some");
    let tool_json = serde_json::to_value(&tool_choice).unwrap();
    assert_eq!(
        tool_json,
        serde_json::json!({
            "type": "tool",
            "name": "read_file",
            "disable_parallel_tool_use": true,
        })
    );
}

#[test]
fn model_request_default_parallel_tool_use_is_true() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8_192)
        .try_build()
        .unwrap();
    assert!(
        request.parallel_tool_use,
        "default must be true so codex-style batching is on by default"
    );
}

#[test]
fn model_request_parallel_tool_use_builder_override() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8_192)
        .parallel_tool_use(false)
        .try_build()
        .unwrap();
    assert!(
        !request.parallel_tool_use,
        "builder override must propagate to ModelRequest.parallel_tool_use"
    );
}

#[test]
fn test_cache_control_on_system_block() {
    let system = build_system_block("You are a helpful assistant.", true)
        .expect("non-empty prompt emits a system block");
    let arr = system.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let block = &arr[0];
    assert_eq!(block["type"], "text");
    assert_eq!(block["text"], "You are a helpful assistant.");
    assert_eq!(block["cache_control"]["type"], "ephemeral");
}

/// Regression for `system.0: cache_control cannot be set for empty text
/// blocks`. Chat sessions enter `complete()` with `system = ""` (see
/// `crates/aura-runtime/src/session/state.rs` `Session::new`), so the
/// build helper must omit the block entirely instead of producing an
/// empty `text` block decorated with `cache_control`.
#[test]
fn build_system_block_returns_none_for_empty_prompt_even_with_caching() {
    assert!(build_system_block("", true).is_none());
    assert!(build_system_block("", false).is_none());
    assert!(build_system_block("   \n\t  ", true).is_none());
}

#[test]
fn test_cache_control_on_last_tool() {
    let tools = vec![
        ToolDefinition::new(
            "fs.read",
            "Read a file",
            serde_json::json!({"type": "object"}),
        ),
        ToolDefinition::new(
            "fs.write",
            "Write a file",
            serde_json::json!({"type": "object"}),
        ),
    ];

    let api_tools = convert_tools_to_api(&tools, true);
    assert_eq!(api_tools.len(), 2);
    assert!(api_tools[0].cache_control.is_none());
    let last_cc = api_tools[1].cache_control.as_ref().unwrap();
    assert_eq!(last_cc["type"], "ephemeral");
}

#[test]
fn test_cache_control_on_last_user_message() {
    let messages = vec![
        Message::user("Hello"),
        Message::assistant("Hi!"),
        Message::user("How are you?"),
    ];

    let api_msgs = convert_messages_to_api(&messages, true);

    let last_user = &api_msgs[2];
    assert_eq!(last_user.role, "user");
    if let ApiContent::Text { cache_control, .. } = &last_user.content[0] {
        let cc = cache_control.as_ref().unwrap();
        assert_eq!(cc["type"], "ephemeral");
    } else {
        panic!("Expected Text content");
    }

    if let ApiContent::Text { cache_control, .. } = &api_msgs[0].content[0] {
        assert!(cache_control.is_none());
    }
}

#[test]
fn test_beta_header_present() {
    let config = AnthropicConfig::new("test-model");
    let provider = AnthropicProvider::new(config).unwrap();

    let system = build_system_block("test", true).expect("non-empty prompt emits a system block");
    let json = serde_json::to_string(&system).unwrap();
    assert!(json.contains("cache_control"));
    assert!(json.contains("ephemeral"));

    assert_eq!(provider.name(), "anthropic");
}

#[test]
fn test_cache_control_omitted_when_prompt_caching_disabled() {
    let system = build_system_block("test", false).expect("non-empty prompt emits a system block");
    let json = serde_json::to_string(&system).unwrap();
    assert!(!json.contains("cache_control"));

    let messages = vec![
        Message::user("Hello"),
        Message::assistant("Hi!"),
        Message::user("How are you?"),
    ];
    let api_msgs = convert_messages_to_api(&messages, false);
    if let ApiContent::Text { cache_control, .. } = &api_msgs[2].content[0] {
        assert!(cache_control.is_none());
    } else {
        panic!("Expected Text content");
    }

    let tools = vec![
        ToolDefinition::new(
            "fs.read",
            "Read a file",
            serde_json::json!({"type": "object"}),
        ),
        ToolDefinition::new(
            "fs.write",
            "Write a file",
            serde_json::json!({"type": "object"}),
        ),
    ];
    let api_tools = convert_tools_to_api(&tools, false);
    assert!(api_tools.iter().all(|tool| tool.cache_control.is_none()));
}

const TEST_DEFAULT_MODEL: &str = "claude-opus-4-6";
const TEST_FALLBACK_MODEL: &str = "claude-sonnet-4-6";

#[test]
fn test_config_with_fallback() {
    let mut config = AnthropicConfig::new(TEST_DEFAULT_MODEL);
    config.fallback_model = Some(TEST_FALLBACK_MODEL.to_string());
    assert_eq!(config.fallback_model, Some(TEST_FALLBACK_MODEL.to_string()));
}

#[test]
fn test_model_chain_without_fallback() {
    let config = AnthropicConfig::new(TEST_DEFAULT_MODEL);
    let provider = AnthropicProvider::new(config).unwrap();
    let chain = provider.model_chain(TEST_DEFAULT_MODEL);
    assert_eq!(chain, vec![TEST_DEFAULT_MODEL]);
}

#[test]
fn test_model_chain_with_fallback() {
    let mut config = AnthropicConfig::new(TEST_DEFAULT_MODEL);
    config.fallback_model = Some(TEST_FALLBACK_MODEL.to_string());
    let provider = AnthropicProvider::new(config).unwrap();
    let chain = provider.model_chain(TEST_DEFAULT_MODEL);
    assert_eq!(chain, vec![TEST_DEFAULT_MODEL, TEST_FALLBACK_MODEL]);
}

#[test]
fn test_model_chain_deduplicates() {
    let mut config = AnthropicConfig::new(TEST_DEFAULT_MODEL);
    config.fallback_model = Some(TEST_DEFAULT_MODEL.to_string());
    let provider = AnthropicProvider::new(config).unwrap();
    let chain = provider.model_chain(TEST_DEFAULT_MODEL);
    assert_eq!(chain, vec![TEST_DEFAULT_MODEL]);
}

#[test]
fn test_api_error_classification() {
    let overloaded: ReasonerError = ApiError::Overloaded {
        message: "529 overloaded".into(),
        retry_after: None,
    }
    .into();
    assert!(overloaded.to_string().contains("529"));

    let credits: ReasonerError = ApiError::InsufficientCredits("402 insufficient".into()).into();
    assert!(credits.to_string().contains("402"));

    let cloudflare: ReasonerError = ApiError::CloudflareBlock("Cloudflare block".into()).into();
    assert!(cloudflare.to_string().contains("Cloudflare"));

    let other: ReasonerError =
        ApiError::Other(ReasonerError::Request("network error".into())).into();
    assert!(other.to_string().contains("network error"));

    // Phase 5: generic 5xx now round-trips into the dedicated
    // `ReasonerError::Transient` variant so callers can detect
    // retryable upstream blips without re-deriving classification
    // from the HTTP status. The body preview the dev loop already
    // surfaces in `task_failed` reasons is preserved.
    let transient_5xx: ReasonerError = ApiError::TransientServer {
        status: 500,
        message: "Anthropic API error: 500 Internal Server Error - body".into(),
    }
    .into();
    match transient_5xx {
        ReasonerError::Transient {
            status,
            ref message,
            retry_after,
        } => {
            assert_eq!(status, 500);
            assert!(
                message.contains("Internal Server Error"),
                "TransientServer should preserve the body preview: {message}"
            );
            assert!(
                retry_after.is_none(),
                "TransientServer carries no Retry-After hint by default"
            );
        }
        other => panic!("TransientServer should map to ReasonerError::Transient, got {other:?}"),
    }
}

#[test]
fn test_overloaded_message_appends_retry_after_hint_when_absent() {
    let err: ReasonerError = ApiError::Overloaded {
        message: "Anthropic API error: 429 Too Many Requests - server busy".into(),
        retry_after: Some(Duration::from_secs(7)),
    }
    .into();
    let msg = err.to_string();
    assert!(
        msg.contains("retry after 7 seconds"),
        "message should include the retry-after hint when not already present: {msg}"
    );
}

#[test]
fn test_overloaded_message_leaves_existing_retry_after_phrase_alone() {
    let err: ReasonerError = ApiError::Overloaded {
        message: "Anthropic API error: 429 - \
                  {\"error\":{\"code\":\"RATE_LIMITED\",\"message\":\"Too many requests. Retry after 7 seconds.\"}}"
            .into(),
        retry_after: Some(Duration::from_secs(7)),
    }
    .into();
    let msg = err.to_string();
    let occurrences = msg.to_ascii_lowercase().matches("retry after").count();
    assert_eq!(
        occurrences, 1,
        "should not double-append the retry-after phrase: {msg}"
    );
}

#[test]
fn test_cloudflare_detection() {
    use super::is_cloudflare_html;
    assert!(is_cloudflare_html(
        r#"<!DOCTYPE html><!--[if lt IE 7]> <html class="no-js ie6 oldie" lang="en-US">"#
    ));
    assert!(is_cloudflare_html(
        r#"<!DOCTYPE html><html><body><p>Your request was blocked by this site's web application firewall (WAF).</p></body></html>"#
    ));
    assert!(!is_cloudflare_html(
        r#"{"error":{"type":"authentication_error","message":"invalid api key"}}"#
    ));
}

/// Process-wide buffer that the global `aura::console` subscriber
/// writes into. Tests acquire [`CONSOLE_CAPTURE_LOCK`] to take
/// exclusive ownership, clear, run their scenario, snapshot, then
/// release. Writes from other parallel tests are filtered out via
/// [`CAPTURE_THREAD_ID`] so they don't pollute the snapshot.
static CONSOLE_CAPTURE_BUF: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// Process-wide lock for tests that capture the `aura::console`
/// transcript. Serializes capture so concurrent `#[test]`s don't
/// interleave their event traffic into the shared buffer.
static CONSOLE_CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CONSOLE_CAPTURE_INIT: std::sync::Once = std::sync::Once::new();

/// Thread ID of the test currently holding [`CONSOLE_CAPTURE_LOCK`].
/// `0` means "no test is capturing" — the writer drops the event in
/// that state, so we don't accumulate events from unrelated tests.
static CAPTURE_THREAD_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn current_thread_id_u64() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

struct CaptureWriter;
impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let target = CAPTURE_THREAD_ID.load(std::sync::atomic::Ordering::Acquire);
        if target != 0 && target == current_thread_id_u64() {
            CONSOLE_CAPTURE_BUF.lock().unwrap().extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Lock-guarded handle on the shared capture buffer. Dropping it
/// releases the lock and unforces ANSI colors. Constructed via
/// [`install_console_capture`].
struct ConsoleCapture {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ConsoleCapture {
    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&CONSOLE_CAPTURE_BUF.lock().unwrap()).to_string()
    }
}

impl Drop for ConsoleCapture {
    fn drop(&mut self) {
        CAPTURE_THREAD_ID.store(0, std::sync::atomic::Ordering::Release);
        colored::control::unset_override();
    }
}

/// Install — exactly once per process — a global tracing subscriber
/// that funnels every event into [`CONSOLE_CAPTURE_BUF`]. Acquire the
/// process-wide capture lock, clear the buffer, force `colored` off,
/// and return a guard. Subsequent capturing tests will block on the
/// lock until this guard drops.
///
/// ## Why a single global subscriber
///
/// Per-test thread-local subscribers (`set_default` / `with_default`)
/// don't survive `tokio` task hops or `tracing`'s callsite-interest
/// cache when other tests run in parallel — events from
/// `info!(target: "aura::console", …)` race against other subscribers
/// installed on other threads, and the cached `Interest` decides
/// whether the event reaches us or them. Owning the dispatcher
/// process-wide and serializing capture sidesteps the race.
fn install_console_capture() -> ConsoleCapture {
    use tracing_subscriber::fmt::layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    CONSOLE_CAPTURE_INIT.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(
            layer()
                .with_ansi(false)
                .with_writer(|| CaptureWriter)
                .with_target(true),
        );
        let _ = subscriber.try_init();
    });

    let lock = CONSOLE_CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    CONSOLE_CAPTURE_BUF.lock().unwrap().clear();
    CAPTURE_THREAD_ID.store(
        current_thread_id_u64(),
        std::sync::atomic::Ordering::Release,
    );
    colored::control::set_override(false);
    ConsoleCapture { _lock: lock }
}

/// Runs a synchronous `#[test]` rather than `#[tokio::test]` so the
/// tracing subscriber stays installed for the entire round-trip and
/// is not torn down or shadowed across `tokio`'s task hops. Capture
/// is serialized via [`install_console_capture`].
#[test]
fn cloudflare_block_round_trip_emits_paired_request_and_failure_blocks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let capture = install_console_capture();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let result = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await.unwrap();

            let body = r#"<!DOCTYPE html><html><body>
                <p>Your request was blocked by this site's web application firewall (WAF).</p>
                <p>Request ID: <code>cf-test-paired</code></p>
            </body></html>"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\ncf-ray: ray-test\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let config = AnthropicConfig {
            default_model: "aura-claude-sonnet-4-6".to_string(),
            timeout_ms: 5_000,
            max_retries: 0,
            backoff_initial_ms: 250,
            backoff_cap_ms: 30_000,
            min_request_interval_ms: 0,
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            fallback_model: None,
            prompt_caching_enabled: true,
            emergency_body_cap_bytes: 0,
            cloudflare_max_retries: 3,
        };

        let provider = AnthropicProvider::new(config).unwrap();
        let request = ModelRequest::builder("aura-claude-sonnet-4-6", "system")
            .message(Message::user("test"))
            .request_kind(ModelRequestKind::Chat)
            .auth_token(Some("test-jwt-token".to_string()))
            .try_build()
            .unwrap();

        let result = provider.complete(request).await;
        server.await.unwrap();
        result
    });

    let captured = capture.snapshot();
    assert!(result.is_err(), "Cloudflare block should surface as error");
    assert!(
        captured.contains("→ POST /v1/messages"),
        "expected paired outbound request block, got transcript:\n{captured}"
    );
    assert!(
        captured.contains("← 403 Forbidden"),
        "expected paired failure block with 403 header, got transcript:\n{captured}"
    );
    assert!(
        captured.contains("cloudflare_block"),
        "expected class label in failure block, got transcript:\n{captured}"
    );
    assert!(
        captured.contains("✗ propagate") || captured.contains("↻ retry"),
        "expected retry-decision continuation line, got transcript:\n{captured}"
    );
}

#[tokio::test]
async fn test_cloudflare_html_maps_to_typed_diagnostic_with_profile() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await.unwrap();

        let body = r#"<!DOCTYPE html><html><body>
            <p>Your request was blocked by this site's web application firewall (WAF).</p>
            <p>Request ID: <code>cf-test-123</code></p>
        </body></html>"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\ncf-ray: ray-test\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let config = AnthropicConfig {
        default_model: "aura-claude-sonnet-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-claude-sonnet-4-6", "system")
        .message(Message::user("test"))
        .request_kind(ModelRequestKind::Chat)
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let err = provider.complete(request).await.unwrap_err();
    server.await.unwrap();

    match err {
        ReasonerError::Transient {
            status, message, ..
        } => {
            assert_eq!(status, 403);
            assert!(message.contains("Cloudflare block"));
            assert!(message.contains("request_id=cf-test-123"));
            assert!(message.contains("kind=Chat"));
            assert!(message.contains("content_signature="));
        }
        other => panic!("expected typed Cloudflare transient diagnostic, got {other:?}"),
    }
}

#[test]
fn test_resolve_thinking_explicit_config() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8192)
        .thinking(ThinkingConfig {
            budget_tokens: 4000,
        })
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL);
    assert!(thinking.is_some());
    let thinking = thinking.unwrap();
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);
}

#[test]
fn test_resolve_thinking_auto_for_capable_model() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL);
    assert!(thinking.is_some());
    let thinking = thinking.unwrap();
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);
}

#[test]
fn test_resolve_thinking_auto_for_aura_alias_capable_model() {
    let request = ModelRequest::builder("aura-claude-opus-4-7", "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "aura-claude-opus-4-7").unwrap();
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);
}

#[test]
fn test_resolve_thinking_uses_enabled_budget_for_older_models() {
    let request = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(8192)
        .thinking(ThinkingConfig {
            budget_tokens: 4000,
        })
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-7-sonnet").unwrap();
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, Some(4000));
}

#[test]
fn test_resolve_thinking_none_for_unsupported_haiku_variants() {
    let request = ModelRequest::builder("aura-claude-haiku-4-5", "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "aura-claude-haiku-4-5");
    assert!(thinking.is_none());
}

#[test]
fn test_resolve_thinking_auto_for_non_claude_model_is_none() {
    let request = ModelRequest::builder("aura-gpt-5-4", "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "aura-gpt-5-4");
    assert!(thinking.is_none());
}

#[test]
fn test_resolve_thinking_none_for_small_budget() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(1024)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL);
    assert!(thinking.is_none());
}

#[test]
fn test_resolve_thinking_none_for_unsupported_claude_3_variants() {
    let request = ModelRequest::builder("claude-3-haiku", "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-haiku");
    assert!(thinking.is_none());
}

// ---------------------------------------------------------------------------
// Phase 2 — `ThinkingEffort` knob (codex `reasoning.effort` analog)
// ---------------------------------------------------------------------------

#[test]
fn thinking_effort_off_emits_no_config() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Off))
        .try_build()
        .unwrap();
    assert!(resolve_thinking(&request, TEST_DEFAULT_MODEL).is_none());
    assert!(resolve_output_config(&request, TEST_DEFAULT_MODEL).is_none());
}

/// Regression for `thinking.adaptive.budget_tokens: Extra inputs are not
/// permitted`. Anthropic's `adaptive` thinking mode (Claude 4 family)
/// rejects `budget_tokens` outright — the model picks its own budget.
/// The Low/Medium/High effort knob must therefore translate to
/// `{"type":"adaptive"}` with no `budget_tokens` for adaptive models.
#[test]
fn thinking_effort_low_adaptive_omits_budget_tokens() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Low))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL).expect("Low emits a config");
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);
    // Low must NOT inherit the forced effort=high override — that's
    // exactly the spiral amplifier we want to cap.
    assert!(resolve_output_config(&request, TEST_DEFAULT_MODEL).is_none());
}

#[test]
fn thinking_effort_medium_adaptive_omits_budget_tokens() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Medium))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL).expect("Medium emits a config");
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);
    assert!(resolve_output_config(&request, TEST_DEFAULT_MODEL).is_none());
}

#[test]
fn thinking_effort_high_adaptive_omits_budget_tokens_but_keeps_output_effort() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8_192)
        .thinking_effort(Some(ThinkingEffort::High))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL).expect("High emits a config");
    assert_eq!(thinking.thinking_type, "adaptive");
    assert_eq!(thinking.budget_tokens, None);

    // High retains the existing forced effort=high override for adaptive.
    let out = resolve_output_config(&request, TEST_DEFAULT_MODEL).expect("High keeps effort=high");
    assert_eq!(out.effort, "high");
}

/// Companion to the adaptive tests: the `enabled` thinking mode
/// (Claude 3.7 sonnet) DOES accept `budget_tokens`, and the effort knob
/// keeps the calibrated 1024 / 4096 / clamped budgets there.
#[test]
fn thinking_effort_low_enabled_sets_1024_budget() {
    let request = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Low))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-7-sonnet").expect("Low emits a config");
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, Some(1024));
}

#[test]
fn thinking_effort_medium_enabled_sets_4096_budget() {
    let request = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Medium))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-7-sonnet").expect("Medium emits a config");
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, Some(4096));
}

#[test]
fn thinking_effort_high_enabled_clamps_budget_to_8192_16000() {
    // Small ceiling: clamp pushes the budget up to 8192.
    let request = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(8_192)
        .thinking_effort(Some(ThinkingEffort::High))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-7-sonnet").expect("High emits a config");
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, Some(8_192));

    // Large ceiling: clamp pulls the budget down to 16000.
    let request_big = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(64_000)
        .thinking_effort(Some(ThinkingEffort::High))
        .try_build()
        .unwrap();
    let thinking_big =
        resolve_thinking(&request_big, "claude-3-7-sonnet").expect("High emits a config");
    assert_eq!(thinking_big.budget_tokens, Some(16_000));
}

#[test]
fn thinking_effort_none_preserves_legacy_max_tokens_gate() {
    // No effort set + max_tokens above the legacy 2048 threshold:
    // legacy auto-enable fires unchanged.
    let auto = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8_192)
        .try_build()
        .unwrap();
    let thinking_auto = resolve_thinking(&auto, TEST_DEFAULT_MODEL).expect("legacy auto-enable");
    assert_eq!(thinking_auto.thinking_type, "adaptive");
    assert_eq!(thinking_auto.budget_tokens, None);
    let out_auto = resolve_output_config(&auto, TEST_DEFAULT_MODEL).expect("legacy effort=high");
    assert_eq!(out_auto.effort, "high");

    // No effort set + max_tokens at/below 2048: legacy gate keeps it off.
    let off = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(2_048)
        .try_build()
        .unwrap();
    assert!(resolve_thinking(&off, TEST_DEFAULT_MODEL).is_none());
}

#[test]
fn thinking_effort_returns_none_for_models_without_thinking_support() {
    // Models that don't support thinking (e.g. Haiku) must stay off
    // regardless of the effort knob — capability detection wins.
    let request = ModelRequest::builder("aura-claude-haiku-4-5", "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::High))
        .try_build()
        .unwrap();
    assert!(resolve_thinking(&request, "aura-claude-haiku-4-5").is_none());
}

#[test]
fn thinking_effort_low_on_older_model_emits_enabled_type() {
    // Sanity: Claude 3.7 sonnet still maps to the "enabled" thinking
    // type, but with the Low budget instead of the auto-derived one.
    let request = ModelRequest::builder("claude-3-7-sonnet", "system")
        .max_tokens(16_384)
        .thinking_effort(Some(ThinkingEffort::Low))
        .try_build()
        .unwrap();
    let thinking = resolve_thinking(&request, "claude-3-7-sonnet").expect("Low emits a config");
    assert_eq!(thinking.thinking_type, "enabled");
    assert_eq!(thinking.budget_tokens, Some(1024));
}

#[test]
fn test_resolve_output_config_only_for_claude_4_thinking_models() {
    let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
        .max_tokens(8192)
        .try_build()
        .unwrap();
    let output = resolve_output_config(&request, TEST_DEFAULT_MODEL).unwrap();
    assert_eq!(output.effort, "high");

    let sonnet_37_output = resolve_output_config(&request, "claude-3-7-sonnet");
    assert!(sonnet_37_output.is_none());
}

/// Regression for Anthropic's May 2026 removal of `thinking.type.enabled`
/// on the Claude 4 family. Dev-loop runs used to escalate the adaptive
/// thinking mode for opus-4 / sonnet-4 to coax visible thinking blocks,
/// which now 400s with
/// `"thinking.type.enabled" is not supported for this model.
///  Use "thinking.type.adaptive" and "output_config.effort" to control
///  thinking behavior.` Every dev-loop ModelRequestKind on an adaptive
/// model must therefore stay on `adaptive` (no `budget_tokens`) and lean
/// on `output_config.effort: "high"` instead.
#[test]
fn dev_loop_kinds_on_adaptive_model_stay_adaptive_with_effort_high() {
    for kind in [
        ModelRequestKind::DevLoopBootstrap,
        ModelRequestKind::DevLoopContinuation,
        ModelRequestKind::ProjectToolTaskExtract,
        ModelRequestKind::ProjectToolSpecGen,
    ] {
        let request = ModelRequest::builder(TEST_DEFAULT_MODEL, "system")
            .max_tokens(8192)
            .request_kind(kind)
            .try_build()
            .unwrap();
        let thinking = resolve_thinking(&request, TEST_DEFAULT_MODEL)
            .unwrap_or_else(|| panic!("{kind:?} should still emit a thinking config"));
        assert_eq!(
            thinking.thinking_type, "adaptive",
            "{kind:?} must NOT escalate to thinking.type=enabled on opus-4/sonnet-4; Anthropic rejects that combination"
        );
        assert_eq!(
            thinking.budget_tokens, None,
            "{kind:?}: adaptive mode rejects budget_tokens",
        );
        let output = resolve_output_config(&request, TEST_DEFAULT_MODEL)
            .unwrap_or_else(|| panic!("{kind:?} should pair adaptive with output_config.effort"));
        assert_eq!(
            output.effort, "high",
            "{kind:?} must keep effort=high so adaptive still produces visible thinking",
        );
    }
}

#[tokio::test]
async fn test_proxy_mode_sends_caching_beta_header() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"test","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-claude-sonnet-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-claude-sonnet-4-6", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await;

    let captured = server.await.unwrap();
    assert!(
        captured.contains("anthropic-beta"),
        "Proxy request should include anthropic-beta header.\nCaptured headers:\n{captured}"
    );
    assert!(
        captured.contains("prompt-caching-2024-07-31"),
        "anthropic-beta header should include prompt-caching beta tag.\nCaptured headers:\n{captured}"
    );
}

#[tokio::test]
async fn test_proxy_mode_forwards_aura_routing_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"test","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-claude-sonnet-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-claude-sonnet-4-6", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .aura_org_id(Some("org-123".to_string()))
        .aura_session_id(Some("session-456".to_string()))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await;

    let captured = server.await.unwrap();
    let captured_lower = captured.to_ascii_lowercase();
    assert!(
        captured_lower.contains("x-aura-org-id: org-123"),
        "Proxy request should include org routing header.\nCaptured request:\n{captured}"
    );
    assert!(
        captured_lower.contains("x-aura-session-id: session-456"),
        "Proxy request should include session routing header.\nCaptured request:\n{captured}"
    );
}

#[tokio::test]
async fn test_complete_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let _server = tokio::spawn(async move {
        loop {
            let Ok((_socket, _)) = listener.accept().await else {
                break;
            };
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    let config = AnthropicConfig {
        default_model: "test-model".to_string(),
        timeout_ms: 200,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("test-model", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let result = provider.complete(request).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, ReasonerError::Timeout),
        "expected Timeout, got: {err:?}"
    );
}

#[tokio::test]
async fn test_proxy_openai_models_fall_back_to_buffered_streaming() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"proxy ok"}],"model":"aura-gpt-4.1","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-gpt-4.1".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-gpt-4.1", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let stream = provider.complete_streaming(request).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;

    let captured = server.await.unwrap();
    assert!(
        !captured.contains(r#""stream":true"#),
        "Buffered fallback should avoid Anthropic SSE requests.\nCaptured request:\n{captured}"
    );

    assert!(
        events.iter().any(|event| matches!(
            event.as_ref().unwrap(),
            StreamEvent::MessageStart { model, .. } if model == "aura-gpt-4.1"
        )),
        "expected a MessageStart with model aura-gpt-4.1 somewhere in the event stream"
    );
    assert!(events.iter().any(|event| matches!(
        event.as_ref().unwrap(),
        StreamEvent::TextDelta { text } if text == "proxy ok"
    )));
    assert!(events.iter().any(|event| matches!(
        event.as_ref().unwrap(),
        StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            output_tokens: 5,
        }
    )));
    assert!(matches!(
        events.last().unwrap().as_ref().unwrap(),
        StreamEvent::MessageStop
    ));
}

#[tokio::test]
async fn test_cross_family_proxy_fallback_buffers_streaming_and_omits_anthropic_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut first_socket, _) = listener.accept().await.unwrap();
        let mut first_buf = vec![0u8; 8192];
        let first_n = first_socket.read(&mut first_buf).await.unwrap();
        let first_request = String::from_utf8_lossy(&first_buf[..first_n]).to_string();
        let overloaded = "HTTP/1.1 529 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"type\":\"error\",\"error\":\"overloaded\"}";
        first_socket.write_all(overloaded.as_bytes()).await.unwrap();

        let (mut second_socket, _) = listener.accept().await.unwrap();
        let mut second_buf = vec![0u8; 8192];
        let second_n = second_socket.read(&mut second_buf).await.unwrap();
        let second_request = String::from_utf8_lossy(&second_buf[..second_n]).to_string();
        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"fallback ok"}],"model":"aura-gpt-5-4","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        second_socket.write_all(response.as_bytes()).await.unwrap();

        (first_request, second_request)
    });

    let config = AnthropicConfig {
        default_model: "claude-opus-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: Some("aura-gpt-5-4".to_string()),
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("claude-opus-4-6", "system")
        .message(Message::user("test"))
        .max_tokens(8192)
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let stream = provider.complete_streaming(request).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;

    let (first_request, second_request) = server.await.unwrap();
    assert!(
        first_request.contains(r#""stream":true"#),
        "Primary Anthropic request should still use SSE.\nCaptured request:\n{first_request}"
    );
    assert!(
        !second_request.contains(r#""stream":true"#),
        "Cross-family fallback should buffer instead of using Anthropic SSE.\nCaptured request:\n{second_request}"
    );
    assert!(
        !second_request.contains("anthropic-beta"),
        "Cross-family fallback should omit Anthropic beta headers.\nCaptured request:\n{second_request}"
    );
    assert!(
        !second_request.contains(r#""thinking":"#),
        "Cross-family fallback should omit Anthropic thinking config.\nCaptured request:\n{second_request}"
    );
    assert!(
        !second_request.contains("output_config"),
        "Cross-family fallback should omit Anthropic output config.\nCaptured request:\n{second_request}"
    );
    assert!(events.iter().any(|event| matches!(
        event.as_ref().unwrap(),
        StreamEvent::TextDelta { text } if text == "fallback ok"
    )));
}

#[tokio::test]
async fn test_proxy_openai_models_omit_prompt_caching_headers_and_fields() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"aura-gpt-4.1","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-gpt-4.1".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-gpt-4.1", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await.unwrap();

    let captured = server.await.unwrap();
    assert!(
        !captured.contains("anthropic-beta"),
        "Proxy OpenAI requests should omit anthropic-beta prompt caching headers.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("cache_control"),
        "Proxy OpenAI requests should omit Anthropic cache_control fields.\nCaptured request:\n{captured}"
    );
}

#[tokio::test]
async fn test_proxy_deepseek_family_uses_provider_hint_and_usage_aliases() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"deepseek-v4-flash","stop_reason":"end_turn","usage":{"prompt_tokens":100,"completion_tokens":25,"prompt_cache_miss_tokens":80,"prompt_cache_hit_tokens":20}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "deepseek-v4-flash".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("deepseek-v4-flash", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .upstream_provider_family(Some("deepseek".to_string()))
        .try_build()
        .unwrap();

    let response = provider.complete(request).await.unwrap();

    let captured = server.await.unwrap();
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("x-aura-upstream-provider-family: deepseek"),
        "Proxy DeepSeek requests should carry the upstream provider family hint.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("anthropic-beta"),
        "Proxy DeepSeek requests should omit Anthropic beta headers.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("cache_control"),
        "Proxy DeepSeek requests should omit Anthropic cache_control fields.\nCaptured request:\n{captured}"
    );
    assert_eq!(response.usage.input_tokens, 100);
    assert_eq!(response.usage.output_tokens, 25);
    assert_eq!(response.usage.cache_creation_input_tokens, Some(80));
    assert_eq!(response.usage.cache_read_input_tokens, Some(20));
}

#[tokio::test]
async fn test_proxy_hint_prefers_anthropic_family_over_model_heuristics() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"aura-gpt-4.1","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-gpt-4.1".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-gpt-4.1", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .upstream_provider_family(Some("anthropic".to_string()))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await.unwrap();

    let captured = server.await.unwrap();
    assert!(
        captured.contains("anthropic-beta"),
        "Explicit Anthropic family hints should enable Anthropic proxy headers even for non-Claude model names.\nCaptured request:\n{captured}"
    );
    assert!(
        captured.contains("cache_control"),
        "Explicit Anthropic family hints should enable Anthropic cache_control fields even for non-Claude model names.\nCaptured request:\n{captured}"
    );
}

#[tokio::test]
async fn test_proxy_hint_prefers_non_anthropic_family_over_model_heuristics_for_streaming() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"proxy ok"}],"model":"claude-opus-4-6","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "claude-opus-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("claude-opus-4-6", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .upstream_provider_family(Some("openai".to_string()))
        .try_build()
        .unwrap();

    let stream = provider.complete_streaming(request).await.unwrap();
    let events = stream.collect::<Vec<_>>().await;

    let captured = server.await.unwrap();
    assert!(
        !captured.contains(r#""stream":true"#),
        "Explicit non-Anthropic family hints should force buffered proxy streaming even for Claude model names.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("anthropic-beta"),
        "Explicit non-Anthropic family hints should suppress Anthropic proxy headers even for Claude model names.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains(r#""thinking":"#),
        "Explicit non-Anthropic family hints should suppress Anthropic thinking config even for Claude model names.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("output_config"),
        "Explicit non-Anthropic family hints should suppress Anthropic output config even for Claude model names.\nCaptured request:\n{captured}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event.as_ref().unwrap(),
            StreamEvent::MessageStart { model, .. } if model == "claude-opus-4-6"
        )),
        "expected a MessageStart with model claude-opus-4-6 somewhere in the event stream"
    );
}

#[tokio::test]
async fn test_proxy_non_anthropic_family_omits_thinking_and_output_config_for_complete() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-opus-4-6","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "claude-opus-4-6".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("claude-opus-4-6", "system")
        .message(Message::user("test"))
        .max_tokens(8192)
        .auth_token(Some("test-jwt-token".to_string()))
        .upstream_provider_family(Some("openai".to_string()))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await.unwrap();

    let captured = server.await.unwrap();
    assert!(
        !captured.contains(r#""thinking":"#),
        "Explicit non-Anthropic family hints should suppress Anthropic thinking config for complete requests.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("output_config"),
        "Explicit non-Anthropic family hints should suppress Anthropic output config for complete requests.\nCaptured request:\n{captured}"
    );
}
#[tokio::test]
async fn test_proxy_openai_models_carry_prompt_cache_key_when_set() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let request_text = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"aura-gpt-4.1","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        request_text
    });

    let config = AnthropicConfig {
        default_model: "aura-gpt-4.1".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-gpt-4.1", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .prompt_cache_key(Some("agent:abc-123".into()))
        .prompt_cache_retention(Some(PromptCacheRetention::Hours24))
        .try_build()
        .unwrap();

    let _ = provider.complete(request).await.unwrap();

    let captured = server.await.unwrap();
    assert!(
        captured.contains(r#""prompt_cache_key":"agent:abc-123""#),
        "Proxy OpenAI requests should carry prompt_cache_key in body.\nCaptured request:\n{captured}"
    );
    assert!(
        captured.contains(r#""prompt_cache_retention":"24h""#),
        "Proxy OpenAI requests should carry prompt_cache_retention in body.\nCaptured request:\n{captured}"
    );
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("x-aura-prompt-cache-key: agent:abc-123"),
        "Proxy OpenAI requests should carry X-Aura-Prompt-Cache-Key header.\nCaptured request:\n{captured}"
    );
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("x-aura-prompt-cache-retention: 24h"),
        "Proxy OpenAI requests should carry X-Aura-Prompt-Cache-Retention header.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("anthropic-beta"),
        "Proxy OpenAI requests should still omit anthropic-beta even with cache key set.\nCaptured request:\n{captured}"
    );
    assert!(
        !captured.contains("cache_control"),
        "Proxy OpenAI requests should still omit cache_control even with cache key set.\nCaptured request:\n{captured}"
    );
}

#[tokio::test]
async fn test_proxy_openai_response_cached_tokens_alias_maps_to_cache_read() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await.unwrap();

        // Anthropic-shape usage block emitted by the router after it
        // flattens OpenAI's `prompt_tokens_details.cached_tokens`.
        let body = r#"{"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"aura-gpt-4.1","stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":25,"cached_tokens":60}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let config = AnthropicConfig {
        default_model: "aura-gpt-4.1".to_string(),
        timeout_ms: 5000,
        max_retries: 0,
        backoff_initial_ms: 250,
        backoff_cap_ms: 30_000,
        min_request_interval_ms: 0,
        base_url: format!("http://127.0.0.1:{}", addr.port()),
        fallback_model: None,
        prompt_caching_enabled: true,
        emergency_body_cap_bytes: 0,
        cloudflare_max_retries: 3,
    };

    let provider = AnthropicProvider::new(config).unwrap();
    let request = ModelRequest::builder("aura-gpt-4.1", "system")
        .message(Message::user("test"))
        .auth_token(Some("test-jwt-token".to_string()))
        .try_build()
        .unwrap();

    let response = provider.complete(request).await.unwrap();
    server.await.unwrap();

    assert_eq!(response.usage.input_tokens, 100);
    assert_eq!(response.usage.output_tokens, 25);
    assert_eq!(response.usage.cache_read_input_tokens, Some(60));
}

#[test]
fn test_anthropic_request_caches_last_tool_when_no_tool_cache_control_set() {
    let tools = vec![
        ToolDefinition::new(
            "fs.read",
            "Read a file",
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        ),
        ToolDefinition::new(
            "fs.write",
            "Write a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "contents": {"type": "string"}
                }
            }),
        ),
    ];

    let api_tools = convert_tools_to_api(&tools, true);
    assert_eq!(api_tools.len(), 2);
    assert!(
        api_tools[0].cache_control.is_none(),
        "First tool should not be cached when no explicit cache_control is set"
    );
    assert_eq!(
        api_tools[1].cache_control,
        Some(serde_json::json!({"type": "ephemeral"})),
        "Last tool should default to ephemeral cache_control when prompt caching is enabled"
    );
}

// ============================================================================
// dedupe_tool_results — safety net for Anthropic's "single tool_result per
// tool_use_id" invariant. See the doc-comment on `dedupe_tool_results` in
// `convert.rs` for full semantics. Each test pins one rule of the contract.
// ============================================================================

fn tool_result(id: &str, body: &str, is_error: bool) -> ApiContent {
    ApiContent::ToolResult {
        tool_use_id: id.to_string(),
        content: body.to_string(),
        is_error: Some(is_error),
        cache_control: None,
    }
}

fn text_block(text: &str) -> ApiContent {
    ApiContent::Text {
        text: text.to_string(),
        cache_control: None,
    }
}

fn count_tool_results_with_id<'a>(
    api_messages: &'a [ApiMessage],
    id: &str,
) -> Vec<(&'a str, Option<bool>)> {
    api_messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ApiContent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == id => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn dedupe_keeps_last_tool_result_for_same_id() {
    let mut api_messages = vec![
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_X", "stale body", true)],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_X", "fresh body", false)],
        },
    ];

    dedupe_tool_results(&mut api_messages);

    let survivors = count_tool_results_with_id(&api_messages, "toolu_X");
    assert_eq!(
        survivors.len(),
        1,
        "exactly one ToolResult for toolu_X should survive"
    );
    assert_eq!(survivors[0].0, "fresh body", "last-write-wins on body");
    assert_eq!(
        survivors[0].1,
        Some(false),
        "last-write-wins on is_error too"
    );
}

#[test]
fn dedupe_preserves_position_of_first_occurrence() {
    let mut api_messages = vec![ApiMessage {
        role: "user".to_string(),
        content: vec![
            text_block("before"),
            tool_result("toolu_X", "old", false),
            text_block("between"),
            tool_result("toolu_X", "new", false),
            text_block("after"),
        ],
    }];

    dedupe_tool_results(&mut api_messages);

    assert_eq!(api_messages.len(), 1);
    let msg = &api_messages[0];
    assert_eq!(
        msg.content.len(),
        4,
        "three text blocks + one surviving ToolResult"
    );

    match &msg.content[1] {
        ApiContent::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            assert_eq!(
                tool_use_id, "toolu_X",
                "ToolResult sits at the FIRST occurrence index (1)"
            );
            assert_eq!(
                content, "new",
                "body still comes from the LAST occurrence (last-write-wins)"
            );
        }
        other => panic!("expected ToolResult at index 1, got {other:?}"),
    }

    assert!(
        matches!(&msg.content[0], ApiContent::Text { text, .. } if text == "before"),
        "leading Text block untouched"
    );
    assert!(
        matches!(&msg.content[2], ApiContent::Text { text, .. } if text == "between"),
        "middle Text block (between the two original ToolResults) shifts up by one"
    );
    assert!(
        matches!(&msg.content[3], ApiContent::Text { text, .. } if text == "after"),
        "trailing Text block survives"
    );
}

#[test]
fn dedupe_across_messages() {
    let mut api_messages = vec![
        ApiMessage {
            role: "user".to_string(),
            content: vec![
                tool_result("toolu_A", "a-old", false),
                text_block("more user context"),
            ],
        },
        ApiMessage {
            role: "assistant".to_string(),
            content: vec![text_block("ack")],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_A", "a-new", false)],
        },
    ];

    dedupe_tool_results(&mut api_messages);

    assert_eq!(
        api_messages.len(),
        2,
        "third user msg was emptied by dedupe and should be dropped"
    );
    assert_eq!(api_messages[0].role, "user");
    assert_eq!(api_messages[1].role, "assistant");

    let survivors = count_tool_results_with_id(&api_messages, "toolu_A");
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0].0, "a-new");

    assert!(
        matches!(&api_messages[0].content[1], ApiContent::Text { text, .. } if text == "more user context"),
        "the unrelated Text block in the kept message is untouched"
    );
}

#[test]
fn dedupe_no_op_when_unique() {
    let mut api_messages = vec![ApiMessage {
        role: "user".to_string(),
        content: vec![
            tool_result("id_A", "a", false),
            tool_result("id_B", "b", false),
            tool_result("id_C", "c", false),
        ],
    }];

    dedupe_tool_results(&mut api_messages);

    assert_eq!(api_messages.len(), 1);
    assert_eq!(api_messages[0].content.len(), 3);
    let ids: Vec<&str> = api_messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ApiContent::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["id_A", "id_B", "id_C"]);
}

#[test]
fn dedupe_drops_emptied_message() {
    let mut api_messages = vec![
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_X", "first body", false)],
        },
        ApiMessage {
            role: "assistant".to_string(),
            content: vec![text_block("intermediate")],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_X", "duplicate body", false)],
        },
    ];

    dedupe_tool_results(&mut api_messages);

    assert_eq!(
        api_messages.len(),
        2,
        "third user message contained ONLY a duplicate ToolResult and must be dropped"
    );
    assert_eq!(api_messages[0].role, "user");
    assert_eq!(api_messages[1].role, "assistant");
    match &api_messages[0].content[0] {
        ApiContent::ToolResult { content, .. } => {
            assert_eq!(
                content, "duplicate body",
                "kept block carries the LAST body even though it sits at the first position"
            );
        }
        other => panic!("expected surviving ToolResult, got {other:?}"),
    }
}

#[test]
fn dedupe_leaves_non_tool_result_blocks_alone() {
    let mut api_messages = vec![
        ApiMessage {
            role: "assistant".to_string(),
            content: vec![
                ApiContent::Thinking {
                    thinking: "deliberating".to_string(),
                    signature: Some("sig-1".to_string()),
                },
                text_block("here's my plan"),
                ApiContent::ToolUse {
                    id: "toolu_X".to_string(),
                    name: "fs.write".to_string(),
                    input: serde_json::json!({"path": "out.txt"}),
                },
            ],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![
                ApiContent::Image {
                    source: ApiImageSource {
                        source_type: "base64".to_string(),
                        media_type: "image/png".to_string(),
                        data: "AAA".to_string(),
                    },
                },
                tool_result("toolu_X", "first body", false),
                text_block("follow-up note"),
            ],
        },
        ApiMessage {
            role: "user".to_string(),
            content: vec![tool_result("toolu_X", "second body", false)],
        },
    ];

    dedupe_tool_results(&mut api_messages);

    assert_eq!(api_messages.len(), 2);

    // Assistant message: Thinking + Text + ToolUse all preserved, in order.
    assert_eq!(api_messages[0].content.len(), 3);
    assert!(matches!(
        &api_messages[0].content[0],
        ApiContent::Thinking { .. }
    ));
    assert!(matches!(
        &api_messages[0].content[1],
        ApiContent::Text { .. }
    ));
    assert!(matches!(
        &api_messages[0].content[2],
        ApiContent::ToolUse { .. }
    ));

    // User message: Image + ToolResult(LAST body) + Text — order preserved,
    // ToolResult held in place at index 1.
    assert_eq!(api_messages[1].content.len(), 3);
    assert!(matches!(
        &api_messages[1].content[0],
        ApiContent::Image { .. }
    ));
    match &api_messages[1].content[1] {
        ApiContent::ToolResult { content, .. } => {
            assert_eq!(content, "second body", "last-write-wins survived dedupe");
        }
        other => panic!("expected ToolResult at index 1, got {other:?}"),
    }
    assert!(
        matches!(&api_messages[1].content[2], ApiContent::Text { text, .. } if text == "follow-up note"),
    );
}

#[test]
fn convert_messages_to_api_invokes_dedupe() {
    let messages = vec![
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_dup".to_string(),
                content: ToolResultContent::Text("old".to_string()),
                is_error: true,
            }],
        ),
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_dup".to_string(),
                content: ToolResultContent::Text("new".to_string()),
                is_error: false,
            }],
        ),
    ];

    let api_messages = convert_messages_to_api(&messages, false);

    let survivors = count_tool_results_with_id(&api_messages, "toolu_dup");
    assert_eq!(
        survivors.len(),
        1,
        "convert_messages_to_api must invoke dedupe_tool_results internally"
    );
    assert_eq!(survivors[0].0, "new");
    assert_eq!(survivors[0].1, Some(false));
}

#[test]
fn dedupe_regression_synthetic_max_tokens_placeholder() {
    // Mirrors the exact production failure: an assistant ToolUse produces a
    // `tool_use_id`, then `handle_max_tokens` (or similar truncation path)
    // synthesizes a fake `tool_result` placeholder for that id, and later
    // the real tool result for the same id lands in the conversation. The
    // pre-fix harness shipped both `tool_result` blocks to Anthropic and
    // got
    //   "messages.4.content.1: each tool_use must have a single result.
    //    Found multiple tool_result blocks with id: toolu_X"
    // back as a 400. The safety net must keep only the real (later) one.
    let messages = vec![
        Message::new(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "toolu_X".to_string(),
                name: "fs.write".to_string(),
                input: serde_json::json!({"path": "out.txt"}),
            }],
        ),
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_X".to_string(),
                content: ToolResultContent::Text("synthetic max_tokens placeholder".to_string()),
                is_error: true,
            }],
        ),
        Message::assistant("Let me retry the write..."),
        Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_X".to_string(),
                content: ToolResultContent::Text("Wrote 10011 bytes to out.txt".to_string()),
                is_error: false,
            }],
        ),
    ];

    let api_messages = convert_messages_to_api(&messages, true);

    let survivors = count_tool_results_with_id(&api_messages, "toolu_X");
    assert_eq!(
        survivors.len(),
        1,
        "exactly one ToolResult for toolu_X should survive the safety-net sweep"
    );
    assert_eq!(
        survivors[0].0, "Wrote 10011 bytes to out.txt",
        "the REAL tool result must win over the synthetic max_tokens placeholder"
    );
    assert_eq!(
        survivors[0].1,
        Some(false),
        "real result is non-error; placeholder's is_error=true must not leak through"
    );
}
