/// Anthropic-shaped LLM provider configuration.
///
/// All requests are routed through `aura-router` (the Aura proxy). Auth is
/// per-request JWT via `ModelRequest.auth_token`; there is no API key on
/// this struct and no provider-direct path.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Default model to use.
    pub default_model: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Maximum retries per model before falling back.
    ///
    /// Overridable via `AURA_LLM_MAX_RETRIES`. Default 8 to give the
    /// per-tool-call streaming retry loop a meaningful budget when a 5xx
    /// hits mid-stream.
    pub max_retries: u32,
    /// Initial backoff before the first retry, in milliseconds. Doubled on
    /// each subsequent retry up to `backoff_cap_ms`. Overridable via
    /// `AURA_LLM_BACKOFF_INITIAL_MS`.
    pub backoff_initial_ms: u64,
    /// Maximum backoff between retries, in milliseconds. Overridable via
    /// `AURA_LLM_BACKOFF_CAP_MS`.
    pub backoff_cap_ms: u64,
    /// Minimum spacing between outbound `/v1/messages` requests, in
    /// milliseconds. `0` disables throttling.
    ///
    /// This is process-wide in the provider implementation. It is primarily
    /// for local/eval automation that routes through the public managed
    /// router edge, where dense agent-loop request bursts can be blocked by
    /// the WAF before aura-router can return a canonical 429/400.
    pub min_request_interval_ms: u64,
    /// Aura-router base URL. Read from `AURA_ROUTER_URL`; defaults to
    /// `https://aura-router.onrender.com`.
    pub base_url: String,
    /// Optional fallback model when the primary is overloaded (429/529).
    pub fallback_model: Option<String>,
    /// Whether Anthropic prompt-caching directives should be attached.
    pub prompt_caching_enabled: bool,
    /// Hard cap on the serialized `/v1/messages` request body, in
    /// bytes. When `> 0` and the body exceeds the cap, the request
    /// is shrunk in-place — first by truncating the largest text /
    /// `tool_result` payload in the last user message, then by
    /// dropping oldest non-system message pairs, then (last resort)
    /// by collapsing the conversation down to the latest user turn —
    /// so the body fits before it is forwarded to the router. `0`
    /// disables the cap entirely.
    ///
    /// **Default**: `524288` (512 KiB). This is a generous proactive
    /// budget that sits well below Cloudflare's effective wire limit
    /// for the managed router edge. Pre-emptive trimming here keeps
    /// the harness from triggering body-size WAF rules in the first
    /// place; the `403`-retry path then tightens the cap by 25% on
    /// each successive attempt, so a Cloudflare block deterministically
    /// converges on a body the edge will accept rather than burning
    /// the retry budget on the same oversized payload.
    ///
    /// Overridable via `AURA_LLM_EMERGENCY_BODY_CAP_BYTES`. Set to
    /// `0` to disable proactive trimming entirely (legacy behaviour).
    pub emergency_body_cap_bytes: usize,

    /// Maximum number of times a Cloudflare 403 may be retried with
    /// a progressively tighter body cap before the error is surfaced
    /// to the caller. Each retry shrinks the effective cap by 25%
    /// (multiplicatively) so a fixed-pattern WAF rule eventually
    /// drops below whatever threshold the edge is enforcing.
    ///
    /// Default `3`. Overridable via `AURA_LLM_CLOUDFLARE_MAX_RETRIES`.
    /// `0` disables 403 retries entirely (the first 403 surfaces as
    /// a terminal error).
    pub cloudflare_max_retries: u32,
}

impl AnthropicConfig {
    /// Build a config from environment variables.
    ///
    /// Reads:
    /// - `AURA_ROUTER_URL` (default `https://aura-router.onrender.com`)
    /// - `AURA_DEFAULT_MODEL` (default `claude-opus-4-6`)
    /// - `AURA_MODEL_TIMEOUT_MS` (default `300000`)
    /// - `AURA_LLM_MAX_RETRIES` (default `8`)
    /// - `AURA_LLM_BACKOFF_INITIAL_MS` (default `250`)
    /// - `AURA_LLM_BACKOFF_CAP_MS` (default `30000`)
    /// - `AURA_LLM_MIN_REQUEST_INTERVAL_MS` (default `0` = disabled)
    /// - `AURA_DEFAULT_FALLBACK_MODEL` (optional)
    /// - `AURA_DISABLE_PROMPT_CACHING` (`1`/`true`/`yes` disables caching)
    /// - `AURA_LLM_EMERGENCY_BODY_CAP_BYTES` (default `524288` = 512 KiB; `0` disables)
    /// - `AURA_LLM_CLOUDFLARE_MAX_RETRIES` (default `3`)
    ///
    /// Never errors — every field has a default.
    #[must_use]
    pub fn from_env() -> Self {
        let base_url = std::env::var("AURA_ROUTER_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://aura-router.onrender.com".to_string());

        let default_model = std::env::var("AURA_DEFAULT_MODEL")
            .or_else(|_| std::env::var("AURA_ANTHROPIC_MODEL"))
            .unwrap_or_else(|_| "claude-opus-4-6".to_string());

        let timeout_ms = std::env::var("AURA_MODEL_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300_000);

        let fallback_model = std::env::var("AURA_DEFAULT_FALLBACK_MODEL")
            .or_else(|_| std::env::var("AURA_ANTHROPIC_FALLBACK_MODEL"))
            .ok()
            .filter(|s| !s.is_empty());

        let prompt_caching_enabled = !matches!(
            std::env::var("AURA_DISABLE_PROMPT_CACHING").ok().as_deref(),
            Some("1" | "true" | "TRUE" | "yes" | "YES")
        );

        let max_retries: u32 = std::env::var("AURA_LLM_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let backoff_initial_ms: u64 = std::env::var("AURA_LLM_BACKOFF_INITIAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250);
        let backoff_cap_ms: u64 = std::env::var("AURA_LLM_BACKOFF_CAP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);
        let min_request_interval_ms: u64 = std::env::var("AURA_LLM_MIN_REQUEST_INTERVAL_MS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // 512 KiB proactive default — generous enough to leave room
        // for prompt-caching headers + a full transcript on long
        // turns, tight enough to stay clear of the managed router's
        // body-size WAF rules. Reject negative / non-numeric values
        // silently (fall back to the default) so a typo in the env
        // never wedges the harness. `0` is a valid explicit "disable"
        // sentinel and is preserved as-is.
        let emergency_body_cap_bytes: usize = std::env::var("AURA_LLM_EMERGENCY_BODY_CAP_BYTES")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(524_288);

        let cloudflare_max_retries: u32 = std::env::var("AURA_LLM_CLOUDFLARE_MAX_RETRIES")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(3);

        Self {
            default_model,
            timeout_ms,
            max_retries,
            backoff_initial_ms,
            backoff_cap_ms,
            min_request_interval_ms,
            base_url,
            fallback_model,
            prompt_caching_enabled,
            emergency_body_cap_bytes,
            cloudflare_max_retries,
        }
    }

    /// Build a config with an explicit default model. Other fields take
    /// the same defaults as [`from_env`].
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            default_model: model.into(),
            timeout_ms: 300_000,
            max_retries: 8,
            backoff_initial_ms: 250,
            backoff_cap_ms: 30_000,
            min_request_interval_ms: 0,
            base_url: "https://aura-router.onrender.com".to_string(),
            fallback_model: None,
            prompt_caching_enabled: true,
            emergency_body_cap_bytes: 524_288,
            cloudflare_max_retries: 3,
        }
    }
}

#[cfg(test)]
mod env_backoff_tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn with_env<F: FnOnce() -> AnthropicConfig>(f: F) -> AnthropicConfig {
        let _lock = ENV_LOCK.lock().expect("env lock poisoned");
        f()
    }

    #[test]
    fn backoff_fields_default_when_env_unset() {
        let cfg = with_env(|| {
            let _g1 = EnvGuard::unset("AURA_LLM_MAX_RETRIES");
            let _g2 = EnvGuard::unset("AURA_LLM_BACKOFF_INITIAL_MS");
            let _g3 = EnvGuard::unset("AURA_LLM_BACKOFF_CAP_MS");
            let _g4 = EnvGuard::unset("AURA_LLM_MIN_REQUEST_INTERVAL_MS");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.max_retries, 8, "default max_retries");
        assert_eq!(cfg.backoff_initial_ms, 250, "default backoff_initial_ms");
        assert_eq!(cfg.backoff_cap_ms, 30_000, "default backoff_cap_ms");
        assert_eq!(cfg.min_request_interval_ms, 0, "default request throttle");
    }

    #[test]
    fn backoff_fields_read_env_overrides() {
        let cfg = with_env(|| {
            let _g1 = EnvGuard::set("AURA_LLM_MAX_RETRIES", "12");
            let _g2 = EnvGuard::set("AURA_LLM_BACKOFF_INITIAL_MS", "500");
            let _g3 = EnvGuard::set("AURA_LLM_BACKOFF_CAP_MS", "60000");
            let _g4 = EnvGuard::set("AURA_LLM_MIN_REQUEST_INTERVAL_MS", "2500");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.max_retries, 12);
        assert_eq!(cfg.backoff_initial_ms, 500);
        assert_eq!(cfg.backoff_cap_ms, 60_000);
        assert_eq!(cfg.min_request_interval_ms, 2_500);
    }

    #[test]
    fn from_env_uses_router_defaults_with_no_env() {
        let cfg = with_env(|| {
            let _g1 = EnvGuard::unset("AURA_ROUTER_URL");
            let _g2 = EnvGuard::unset("AURA_DEFAULT_MODEL");
            let _g3 = EnvGuard::unset("AURA_ANTHROPIC_MODEL");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.base_url, "https://aura-router.onrender.com");
        assert_eq!(cfg.default_model, "claude-opus-4-6");
    }

    #[test]
    fn emergency_body_cap_defaults_to_512kib() {
        let cfg = with_env(|| {
            let _g = EnvGuard::unset("AURA_LLM_EMERGENCY_BODY_CAP_BYTES");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.emergency_body_cap_bytes, 524_288);
    }

    #[test]
    fn emergency_body_cap_reads_env_value() {
        let cfg = with_env(|| {
            let _g = EnvGuard::set("AURA_LLM_EMERGENCY_BODY_CAP_BYTES", "1048576");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.emergency_body_cap_bytes, 1_048_576);
    }

    #[test]
    fn emergency_body_cap_zero_explicitly_disables() {
        let cfg = with_env(|| {
            let _g = EnvGuard::set("AURA_LLM_EMERGENCY_BODY_CAP_BYTES", "0");
            AnthropicConfig::from_env()
        });
        assert_eq!(
            cfg.emergency_body_cap_bytes, 0,
            "explicit `0` must be honoured as a disable sentinel"
        );
    }

    #[test]
    fn emergency_body_cap_garbage_value_falls_back_to_default() {
        let cfg = with_env(|| {
            let _g = EnvGuard::set("AURA_LLM_EMERGENCY_BODY_CAP_BYTES", "not-a-number");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.emergency_body_cap_bytes, 524_288);
    }

    #[test]
    fn cloudflare_max_retries_defaults_to_three() {
        let cfg = with_env(|| {
            let _g = EnvGuard::unset("AURA_LLM_CLOUDFLARE_MAX_RETRIES");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.cloudflare_max_retries, 3);
    }

    #[test]
    fn cloudflare_max_retries_reads_env_value() {
        let cfg = with_env(|| {
            let _g = EnvGuard::set("AURA_LLM_CLOUDFLARE_MAX_RETRIES", "5");
            AnthropicConfig::from_env()
        });
        assert_eq!(cfg.cloudflare_max_retries, 5);
    }
}
