//! `create_router` + the middleware-stack assembly for the gateway.
//!
//! Renamed from `router/build.rs` in Phase C / Commit 4 so the
//! dispatch root [`super`] only owns module declarations and the
//! re-exports of [`super::RouterState`] / [`create_router`]. The
//! per-endpoint handler bundles live under
//! [`super::handlers`] and are pulled in here purely as imports —
//! no handler logic lives in this file.
//!
//! Co-located with the route-mounting layer:
//!
//! - [`create_router`] — splits the public sub-router (just
//!   `/health`) from the protected sub-router (everything else)
//!   so the auth middleware applies uniformly.
//! - Middleware helpers: [`build_cors_layer`],
//!   [`is_loopback_origin`], [`build_global_governor`],
//!   [`build_strict_governor`], [`ensure_connect_info`],
//!   [`inbound_failure_observer_mw`].
//! - The terminal-upgrade delegate [`terminal_ws_handler`] (forwards
//!   to `aura_terminal::ws::handle_terminal_ws` — see
//!   `crate::terminal` re-export) and the `/health` handler.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, ConnectInfo, DefaultBodyLimit, State},
    http::{
        header::{self, HeaderName},
        HeaderValue, Method, Request, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::inbound_console::{self, reason_for_status, InboundFailureView, InboundRequestView};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{warn, Level};

use crate::terminal;

use super::auth_mw;
use super::handlers::files::{list_files_handler, read_file_handler, resolve_workspace_handler};
use super::handlers::memory;
use super::handlers::processes::{
    create_process_handler, delete_process_handler, get_process_handler, list_process_runs_handler,
    list_processes_handler, trigger_process_handler, update_process_handler,
};
use super::handlers::run::{
    run_list_handler, run_pause_handler, run_start_handler, run_status_handler, run_stop_handler,
};
use super::handlers::run_ws::{self, run_ws_handler};
use super::handlers::secrets::{
    delete_secret_handler, get_secret_handler, list_secrets_handler, put_secret_handler,
};
use super::handlers::skills;
use super::handlers::tool_permissions::{
    get_agent_tool_permissions_handler, get_agent_tools_handler, get_user_tool_defaults_handler,
    put_agent_tool_permissions_handler, put_user_tool_defaults_handler,
};
use super::handlers::tx::{
    get_head_handler, scan_record_handler, submit_tx_handler, tx_status_handler,
};
use super::RouterState;

/// Create the router.
///
/// The router is split into two halves:
///
/// - A **public** sub-router that currently only exposes `GET /health`
///   for liveness probes.
/// - A **protected** sub-router that layers every other route behind the
///   [`auth_mw::require_bearer_mw`] middleware via `.route_layer(...)` so
///   unauthenticated callers are rejected with `401` before any handler
///   logic runs. Using `route_layer` (not `layer`) keeps the middleware
///   scoped to the matched routes and lets `.fallback` still apply
///   uniformly across both halves. (Security audit — phase 1.)
///
/// The auth layer is only attached when [`crate::config::NodeConfig::require_auth`]
/// is `true` (driven by `AURA_NODE_REQUIRE_AUTH`). When auth is disabled
/// the protected sub-router is still structurally separate — matching
/// the public / protected split for ordering and body-limit purposes —
/// but every request is allowed through without a token check.
pub fn create_router(state: RouterState) -> Router {
    // Per-route body limits — tighter ceilings for endpoints that have
    // no legitimate reason to accept a large body. Each one is a
    // *layer* so it overrides the 1 MiB global default applied at the
    // bottom of this function. Phase 9 of the security audit.
    let body_limit_1k = DefaultBodyLimit::max(1024);
    let body_limit_16k = DefaultBodyLimit::max(16 * 1024);
    let body_limit_4k = DefaultBodyLimit::max(4 * 1024);

    let public = Router::new().route("/health", get(health_handler).route_layer(body_limit_1k));

    // Mutating JSON endpoints get a stricter per-IP governor (5/sec,
    // burst 10) so a misbehaving client can't flood writes even if
    // they stay under the global 30/sec cap. See `build_strict_governor`.
    let strict_governor_layer = GovernorLayer {
        config: build_strict_governor(),
    };

    // Strict-rate-limit sub-router: `/tx`, `/v1/run`, and the
    // `:id/pause` + `:id/stop` path params. Pause/stop use a 4 KiB
    // body limit for tiny JSON payloads; `/tx` and `/v1/run` keep
    // the 1 MiB default because legitimate requests can be large.
    let strict_small_body = Router::new()
        .route("/v1/run/:run_id/pause", post(run_pause_handler))
        .route("/v1/run/:run_id/stop", post(run_stop_handler))
        .route_layer(body_limit_4k);

    let strict_default_body = Router::new()
        .route("/tx", post(submit_tx_handler))
        .route("/v1/run", post(run_start_handler));

    let strict = strict_small_body
        .merge(strict_default_body)
        .route_layer(strict_governor_layer);

    let protected = Router::new()
        .route(
            "/api/files",
            get(list_files_handler).route_layer(body_limit_16k),
        )
        .route(
            "/api/read-file",
            get(read_file_handler).route_layer(body_limit_16k),
        )
        .route(
            "/workspace/resolve",
            get(resolve_workspace_handler).route_layer(body_limit_16k),
        )
        .route(
            "/tx/status/:agent_id/:tx_id",
            get(tx_status_handler).route_layer(body_limit_1k),
        )
        .route(
            "/agents/:agent_id/head",
            get(get_head_handler).route_layer(body_limit_1k),
        )
        .route(
            "/agents/:agent_id/record",
            get(scan_record_handler).route_layer(body_limit_16k),
        )
        .route(
            "/users/:user_id/tool-defaults",
            get(get_user_tool_defaults_handler).put(put_user_tool_defaults_handler),
        )
        .route(
            "/agents/:agent_id/tool-permissions",
            get(get_agent_tool_permissions_handler).put(put_agent_tool_permissions_handler),
        )
        .route("/agents/:agent_id/tools", get(get_agent_tools_handler))
        .route("/ws/terminal", get(terminal_ws_handler))
        .route("/stream/:run_id", get(run_ws_handler))
        .route(
            "/v1/run/list",
            get(run_list_handler).route_layer(body_limit_1k),
        )
        .route(
            "/v1/run/:run_id/status",
            get(run_status_handler).route_layer(body_limit_1k),
        )
        // Memory CRUD (canonical paths)
        .route(
            "/memory/:agent_id/facts",
            get(memory::list_facts).post(memory::create_fact),
        )
        .route(
            "/memory/:agent_id/facts/:id",
            get(memory::get_fact)
                .put(memory::update_fact)
                .delete(memory::delete_fact),
        )
        .route(
            "/memory/:agent_id/facts/by-key/:key",
            get(memory::get_fact_by_key),
        )
        .route(
            "/memory/:agent_id/events",
            get(memory::list_events).post(memory::create_event),
        )
        .route(
            "/memory/:agent_id/events/:id",
            axum::routing::delete(memory::delete_event),
        )
        .route(
            "/memory/:agent_id/events/bulk-delete",
            post(memory::bulk_delete_events),
        )
        .route(
            "/memory/:agent_id/procedures",
            get(memory::list_procedures).post(memory::create_procedure),
        )
        .route(
            "/memory/:agent_id/procedures/:id",
            get(memory::get_procedure)
                .put(memory::update_procedure)
                .delete(memory::delete_procedure),
        )
        .route("/memory/:agent_id/snapshot", get(memory::snapshot))
        .route("/memory/:agent_id/wipe", post(memory::wipe))
        .route("/memory/:agent_id/stats", get(memory::stats))
        .route(
            "/memory/:agent_id/continuity",
            get(memory::get_continuity_config).put(memory::update_continuity_config),
        )
        .route(
            "/memory/:agent_id/retrieval/latest",
            get(memory::latest_retrieval_trace),
        )
        .route("/memory/:agent_id/consolidate", post(memory::consolidate))
        // Memory aliases (aura-os proxy sends /api/agents/:id/memory/...)
        .route(
            "/api/agents/:agent_id/memory",
            get(memory::snapshot).delete(memory::wipe),
        )
        .route(
            "/api/agents/:agent_id/memory/facts",
            get(memory::list_facts).post(memory::create_fact),
        )
        .route(
            "/api/agents/:agent_id/memory/facts/:id",
            get(memory::get_fact)
                .put(memory::update_fact)
                .delete(memory::delete_fact),
        )
        .route(
            "/api/agents/:agent_id/memory/facts/by-key/:key",
            get(memory::get_fact_by_key),
        )
        .route(
            "/api/agents/:agent_id/memory/events",
            get(memory::list_events).post(memory::create_event),
        )
        .route(
            "/api/agents/:agent_id/memory/events/:id",
            axum::routing::delete(memory::delete_event),
        )
        .route(
            "/api/agents/:agent_id/memory/events/bulk-delete",
            post(memory::bulk_delete_events),
        )
        .route(
            "/api/agents/:agent_id/memory/procedures",
            get(memory::list_procedures).post(memory::create_procedure),
        )
        .route(
            "/api/agents/:agent_id/memory/procedures/:id",
            get(memory::get_procedure)
                .put(memory::update_procedure)
                .delete(memory::delete_procedure),
        )
        .route("/api/agents/:agent_id/memory/stats", get(memory::stats))
        .route(
            "/api/agents/:agent_id/memory/continuity",
            get(memory::get_continuity_config).put(memory::update_continuity_config),
        )
        .route(
            "/api/agents/:agent_id/memory/retrieval/latest",
            get(memory::latest_retrieval_trace),
        )
        .route(
            "/api/agents/:agent_id/memory/consolidate",
            post(memory::consolidate),
        )
        // Skills CRUD
        .route(
            "/api/skills",
            get(skills::list_skills).post(skills::create_skill),
        )
        .route("/api/skills/:name", get(skills::get_skill))
        .route("/api/skills/:name/activate", post(skills::activate_skill))
        // Per-agent skill installations
        .route(
            "/api/agents/:agent_id/skills",
            get(skills::list_agent_skills).post(skills::install_agent_skill),
        )
        .route(
            "/api/agents/:agent_id/skills/:name",
            axum::routing::delete(skills::uninstall_agent_skill),
        )
        // Legacy compatibility aliases for older harness callers.
        .route(
            "/api/harness/agents/:agent_id/skills",
            get(skills::list_agent_skills).post(skills::install_agent_skill),
        )
        .route(
            "/api/harness/agents/:agent_id/skills/:name",
            axum::routing::delete(skills::uninstall_agent_skill),
        )
        // In-TEE secrets vault (Swarm TEE phase 6). Proxied by the swarm
        // gateway as /v1/agents/:id/secrets[/:name]; values are sealed at
        // rest under the state DEK and only leave the vault on explicit
        // ?reveal=true reads.
        .route("/secrets", get(list_secrets_handler))
        .route(
            "/secrets/:name",
            get(get_secret_handler)
                .put(put_secret_handler)
                .delete(delete_secret_handler)
                .route_layer(body_limit_16k),
        )
        // In-TEE processes / automations (Swarm TEE phase 7). The
        // definitions (cron + prompt + config) and run history are
        // sealed at rest and only readable through this authenticated
        // in-VM API; the sole off-VM export is the trigger-metadata
        // seam (`ProcessStore::trigger_metadata`), wired up in the
        // next phase. `/v1` prefix matches the `/v1/run` route family.
        .route(
            "/v1/processes",
            get(list_processes_handler).post(create_process_handler),
        )
        .route(
            "/v1/processes/:id",
            get(get_process_handler)
                .put(update_process_handler)
                .delete(delete_process_handler),
        )
        .route("/v1/processes/:id/trigger", post(trigger_process_handler))
        .route(
            "/v1/processes/:id/runs",
            get(list_process_runs_handler).route_layer(body_limit_16k),
        )
        .merge(strict);

    let protected = if state.config.require_auth {
        protected.route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_mw::require_bearer_mw,
        ))
    } else {
        protected
    };

    Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state)
        // Outermost observability layer: emit a paired `→ <method>
        // <path>` / `← <status> <reason>` block under the `aura::console`
        // target whenever an inbound request is rejected (non-2xx).
        // Sits outside `TraceLayer` so it observes the final response
        // status produced by every inner layer (auth, governor, body
        // limit, timeout, handler validation). Health probes are
        // suppressed inside the middleware to avoid kubelet-style noise.
        .layer(axum::middleware::from_fn(inbound_failure_observer_mw))
        // Security + observability layers (Wave 5 / T1 + phase 9).
        //
        // Order matters: `.layer(X)` wraps the existing stack, so the
        // *last* `.layer()` call runs first on an incoming request.
        // The stack from outermost (first seen) to innermost is:
        //   TraceLayer -> TimeoutLayer -> CorsLayer ->
        //   DefaultBodyLimit -> ConnectInfo-fallback ->
        //   GlobalConcurrencyLimitLayer -> GovernorLayer (global) ->
        //   (router + per-route strict governor + per-route body limits).
        //
        // Per-route body-limit layers on specific endpoints (e.g.
        // `/health` at 1 KiB, the GET query-param handlers at 16 KiB,
        // the small-JSON POSTs at 4 KiB) override the 1 MiB default
        // that applies to everything else. This keeps the 1 MiB
        // ceiling as a safety net for the few legitimately-large
        // endpoints (`/tx`, `/v1/run`) while throwing 413
        // early for everything that has no business seeing a megabyte
        // of body.
        //
        // `GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT_REQUESTS)`
        // uses a shared `Arc<Semaphore>` — cloning the layer reuses
        // the same semaphore, which is what we need when axum clones
        // the router per connection. A plain `ConcurrencyLimitLayer`
        // would allocate a fresh semaphore per clone and defeat the
        // cap entirely.
        //
        // The `ensure_connect_info` fallback inserts
        // `ConnectInfo<SocketAddr>` into request extensions when it's
        // absent. Production serves with
        // `into_make_service_with_connect_info::<SocketAddr>()` so the
        // real peer is already there; this layer is a no-op in that
        // path. In `oneshot` tests we don't run through a listener,
        // so the fallback keeps the governor's `PeerIpKeyExtractor`
        // from rejecting requests with `UnableToExtractKey`.
        .layer(GovernorLayer {
            config: build_global_governor(),
        })
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT_REQUESTS))
        .layer(axum::middleware::from_fn(ensure_connect_info))
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1 MiB
        .layer(build_cors_layer())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        // Phase 4 (security audit): explicit TraceLayer levels instead
        // of `TraceLayer::new_for_http()`. `tower_http`'s default span
        // already omits request headers — it only records method / uri
        // / version — so the `Authorization` header never enters our
        // log pipeline through this layer. The explicit level setters
        // make that intent auditable: if a future contributor swaps in
        // a custom `make_span_with`, they have to deliberately opt
        // into header logging (and redact Authorization) instead of
        // picking it up from the default.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

/// Build the CORS layer from the `AURA_ALLOWED_ORIGINS` environment variable.
///
/// If set, parses a comma-separated list of exact origin values (e.g.
/// `https://aura.example,https://console.aura.example`). If unset, defaults
/// to a loopback predicate accepting `http://localhost:*` and
/// `http://127.0.0.1:*`, which is the conservative choice for local dev.
///
/// Non-loopback origins are denied by default — operators must opt in via
/// the env var.
fn build_cors_layer() -> CorsLayer {
    let allow_origin = match std::env::var("AURA_ALLOWED_ORIGINS") {
        Ok(raw) if !raw.trim().is_empty() => {
            let values: Vec<HeaderValue> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|origin| match HeaderValue::from_str(origin) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        warn!(origin = %origin, error = %e, "ignoring invalid AURA_ALLOWED_ORIGINS entry");
                        None
                    }
                })
                .collect();
            if values.is_empty() {
                AllowOrigin::predicate(is_loopback_origin)
            } else {
                AllowOrigin::list(values)
            }
        }
        _ => AllowOrigin::predicate(is_loopback_origin),
    };

    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            HeaderName::from_static("x-requested-with"),
        ])
        .allow_origin(allow_origin)
}

/// Predicate that accepts only loopback origins (localhost / 127.0.0.1 / ::1)
/// on any port, using `http` or `https` scheme. Used as the default when
/// `AURA_ALLOWED_ORIGINS` is unset.
fn is_loopback_origin(origin: &HeaderValue, _req_parts: &axum::http::request::Parts) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // Strip the optional port segment so `localhost:3000` matches just as
    // well as bare `localhost`.
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

// === Terminal WebSocket ===

async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<RouterState>,
) -> axum::response::Response {
    let Some(permit) = run_ws::try_acquire_ws_slot(&state.ws_slots) else {
        warn!(
            cap = run_ws::MAX_WS_CONNS_PER_NODE,
            "Refusing /ws/terminal upgrade: WS connection cap reached"
        );
        inbound_console::ws_rejection_line(
            "upgrade.terminal",
            "slot_full",
            Some(&format!("cap={}", run_ws::MAX_WS_CONNS_PER_NODE)),
        );
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    ws.on_upgrade(move |socket| async move {
        // `permit` is moved into the per-socket task so the slot
        // is only released when the socket task actually exits.
        terminal::handle_terminal_ws(socket).await;
        drop(permit);
    })
    .into_response()
}

// === Health ===

/// Return a liveness/readiness response with version + execution guardrails.
///
/// The tool-policy fields (`run_command_enabled`, `shell_enabled`,
/// `allowed_commands`, `binary_allowlist`) expose the effective executor config so
/// external consumers can diff the running harness's policy against
/// what they need. Historically the `aura-os-desktop` external-harness
/// probe relied on `run_command_enabled` to fail fast when the
/// operator forgot `AURA_AUTONOMOUS_DEV_LOOP=1`; the fields are now
/// diagnostics for the command execution guardrails, not catalog visibility.
///
/// The response is deliberately unauthenticated (matches the old
/// minimal-health behaviour) because the information is non-sensitive:
/// anyone who can already reach the health port can trivially discover
/// the same facts by sending any tool invocation and observing the
/// denial. Fields are additive — a missing field in older harness
/// versions means "unknown", and the desktop treats unknown as a warn
/// (not a hard-fail) so mixed-version fleets keep working.
async fn health_handler(State(state): State<RouterState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        // Build-time git commit baked into the container image (see
        // Dockerfile ARG GIT_SHA). Absent for local/non-container builds.
        "git_sha": std::env::var("AURA_HARNESS_GIT_SHA").ok().filter(|s| !s.is_empty()),
        "run_command_enabled": state.tool_config.command.enabled,
        "shell_enabled": state.tool_config.command.allow_shell,
        "allowed_commands": state.tool_config.command.command_allowlist,
        "binary_allowlist": state.tool_config.command.binary_allowlist,
    }))
}

// === Rate limiting / concurrency helpers (phase 9) ===

/// Maximum number of in-flight HTTP requests the node will serve
/// concurrently before new requests wait on the
/// [`GlobalConcurrencyLimitLayer`] semaphore. Each pending request
/// occupies a tokio task plus its body buffer, so this caps worst-case
/// memory+task pressure on the runtime.
pub(crate) const MAX_IN_FLIGHT_REQUESTS: usize = 256;

/// Concrete type of the governor config we construct — spelled out so
/// helper builders can return something that the `GovernorLayer` field
/// accepts. `PeerIpKeyExtractor` is the default when the `axum`
/// feature is enabled, `NoOpMiddleware<QuantaInstant>` is the default
/// middleware `GovernorConfigBuilder` produces.
type GovernorCfg = tower_governor::governor::GovernorConfig<
    tower_governor::key_extractor::PeerIpKeyExtractor,
    governor::middleware::NoOpMiddleware<governor::clock::QuantaInstant>,
>;

/// Build the router-wide rate-limit config.
///
/// 30 requests/sec with a burst of 60, keyed on peer IP address.
/// Fresh per `create_router` call so different test routers don't
/// share a limiter — production only calls `create_router` once.
///
/// INVARIANT: both `per_millisecond` and `burst_size` are hard-coded
/// non-zero integer literals, so `GovernorConfigBuilder::finish()`
/// cannot fail here; the `.expect(...)` below is a
/// provably-infallible-at-compile-time assertion. Covered by
/// `gateway::tests::test_global_governor_config_is_valid`.
fn build_global_governor() -> Arc<GovernorCfg> {
    Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(1000 / 30) // ≈30 req/sec sustained
            .burst_size(60)
            .finish()
            .expect("global governor config should be valid"),
    )
}

/// Stricter rate limit for mutating endpoints (`/tx`, `/v1/run`,
/// `:id/pause`, `:id/stop`). 5/sec burst 10.
///
/// INVARIANT: same reasoning as [`build_global_governor`] — hard-coded
/// non-zero integer literals make the `.expect(...)` infallible by
/// construction.
fn build_strict_governor() -> Arc<GovernorCfg> {
    Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(200) // 5 req/sec sustained
            .burst_size(10)
            .finish()
            .expect("strict governor config should be valid"),
    )
}

/// Inject a default `ConnectInfo<SocketAddr>` into request extensions
/// when one isn't already present.
///
/// Production uses `into_make_service_with_connect_info::<SocketAddr>()`
/// which inserts the real peer address before the request reaches this
/// layer, so this is a no-op in that code path. In `oneshot` tests
/// there is no listener, so without a fallback the governor's
/// `PeerIpKeyExtractor` would error out with `UnableToExtractKey`
/// (which tower_governor surfaces as `500 Internal Server Error`) on
/// every request. Injecting a loopback default means every oneshot
/// request is attributed to the same synthetic "client", which is
/// exactly what the rate-limit test wants.
async fn ensure_connect_info(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.extensions().get::<ConnectInfo<SocketAddr>>().is_none() {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
    }
    next.run(req).await
}

/// Path prefixes whose rejections are intentionally suppressed from the
/// `aura::console` visual transcript. Today this is just `/health` —
/// kubelet, load-balancer, and uptime probes hit it on a fixed cadence
/// and a non-2xx there (e.g. brief startup window) would otherwise
/// flood the log with paired blocks before the operator can see
/// anything else.
const INBOUND_OBSERVE_SKIP_PATHS: &[&str] = &["/health"];

/// Outermost axum middleware that surfaces blocked / rejected inbound
/// requests under the `aura::console` target.
///
/// Captures `method` / `path` / peer / `Content-Length` / start-instant
/// before forwarding the request, awaits the response, and — when the
/// final status is `>= 400` and the path is not in
/// [`INBOUND_OBSERVE_SKIP_PATHS`] — emits a paired
/// [`inbound_console::inbound_request_summary_block`] +
/// [`inbound_console::inbound_failure_block`] so the visual transcript
/// shows the rejection symmetric to the existing outbound `→ POST` /
/// `← <status>` LLM-call blocks.
///
/// Successful (2xx/3xx) responses stay quiet to keep transcript noise
/// low; the existing `TraceLayer` continues to record them through
/// the default formatter for grep-friendly per-request audit.
async fn inbound_failure_observer_mw(req: Request<Body>, next: Next) -> Response {
    let started_at = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let body_bytes = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    let response = next.run(req).await;
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }
    if INBOUND_OBSERVE_SKIP_PATHS
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
    {
        return response;
    }

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    let status_code = status.as_u16();
    let status_text = status
        .canonical_reason()
        .map_or_else(|| status.to_string(), str::to_string);
    let reason = reason_for_status(status_code);

    inbound_console::inbound_request_summary_block(InboundRequestView {
        method: method.as_str(),
        path: &path,
        peer,
        body_bytes,
    });
    inbound_console::inbound_failure_block(InboundFailureView {
        status_code,
        status_text: &status_text,
        reason,
        elapsed_ms,
        peer,
        body_preview: None,
    });
    response
}
