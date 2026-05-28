//! Embedded HTTP server for TUI mode with `/health` plus bearer-gated file APIs.
//!
//! The terminal harness runs as whatever user launched it, which means
//! this server — if reachable from a browser on the same host — can
//! speak for that user. Two controls close that hole:
//!
//! 1. **Per-launch bearer token.** A random 32-byte hex token is minted
//!    every time [`start_api_server`] is called and every non-`/health`
//!    request must present `Authorization: Bearer <token>`. The token
//!    is logged to stderr once, in the same vein as `jupyter notebook`,
//!    so the operator can copy it into tooling. It is never persisted.
//!
//! 2. **Sandboxed file access.** The `/api/files` and `/api/read-file`
//!    handlers route every incoming path through [`aura_tools::Sandbox`],
//!    which canonicalises the workspace root and the candidate path
//!    before comparing prefixes. That catches both plain `../` traversal
//!    and symlinks / junctions whose real target lives outside the
//!    workspace.
//!
//! The directory-walking and capped-read logic lives in
//! [`aura_runtime::files_api`] — the TUI server and the aura-runtime HTTP
//! router share the same implementation so a change to the ignore list
//! or the read cap only needs to land in one place. This module owns
//! the bearer middleware, the sandbox-aware path resolution, and the
//! wire-format mapping from [`aura_runtime::files_api::WalkedEntry`] onto
//! the TUI JSON contract.

use aura_runtime::auth::check_bearer;
use aura_runtime::files_api::{self, ReadOutcome, WalkedEntry, MAX_READ_BYTES, MAX_WALK_DEPTH};
use aura_terminal::UiCommand;
use aura_tools::Sandbox;
use axum::{
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

/// Default API server port
const API_PORT: u16 = 8080;

/// Fallback ports to try if the default is busy
const FALLBACK_PORTS: &[u16] = &[8081, 8082, 8090, 3000];

/// State shared by the embedded server's handlers.
///
/// Holds the per-launch bearer token and the sandbox used to clamp all
/// file-access paths to the workspace root. Both are wrapped in `Arc`
/// so `axum::Router::with_state` can clone cheaply per request.
#[derive(Clone)]
struct ApiState {
    /// Expected bearer token (constant-time compared against the header).
    expected_token: Arc<String>,
    /// Canonicalising path sandbox scoped to the workspace root.
    sandbox: Arc<Sandbox>,
}

/// Information returned from [`start_api_server`].
///
/// `url` is the URL the server is listening on; `token` is the
/// per-launch bearer token. External callers (browsers, curl) must
/// include `Authorization: Bearer <token>` on every non-`/health`
/// request.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields are part of the public handle shape;
                    // consumers outside this crate (e.g. future IPC wiring) may read them.
pub struct ApiServerHandle {
    pub url: String,
    pub token: String,
}

/// Start the embedded API server.
///
/// `workspace_root` is the directory beyond which file-access handlers
/// may not read. When it does not exist the caller is expected to have
/// created it already — otherwise [`Sandbox::new`] will attempt to
/// create it on our behalf.
pub async fn start_api_server(
    cmd_tx: mpsc::Sender<UiCommand>,
    workspace_root: PathBuf,
) -> Option<ApiServerHandle> {
    let sandbox = match Sandbox::new(&workspace_root) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!(
                error = %e,
                root = %workspace_root.display(),
                "Failed to initialise API server sandbox"
            );
            let _ = cmd_tx.try_send(UiCommand::ShowError(format!(
                "API server failed to initialise sandbox: {e}"
            )));
            return None;
        }
    };

    // The embedded TUI API server follows the same `AURA_NODE_REQUIRE_AUTH`
    // gate as the aura-runtime HTTP server. When auth is disabled we mint
    // no token, skip the bearer middleware, and suppress the stderr
    // banner so unattended local dev workflows don't get log noise.
    let require_auth = auth_required_from_env();
    let token = if require_auth {
        generate_token()
    } else {
        String::new()
    };
    let state = ApiState {
        expected_token: Arc::new(token.clone()),
        sandbox,
    };

    // `/health` stays anonymous for liveness probes; everything else
    // sits behind the bearer middleware. Using `route_layer` scopes
    // the middleware to the matched routes, so `merge`ing with the
    // public router leaves `/health` untouched.
    let public = Router::new().route("/health", get(api_health_handler));
    let protected = Router::new()
        .route("/api/files", get(api_list_files_handler))
        .route("/api/read-file", get(api_read_file_handler));
    let protected = if require_auth {
        protected.route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer_mw,
        ))
    } else {
        protected
    };

    let app = Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let ports_to_try = std::iter::once(API_PORT).chain(FALLBACK_PORTS.iter().copied());

    for port in ports_to_try {
        let addr = format!("127.0.0.1:{port}");
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                let url = format!("http://{addr}");
                info!(%url, "API server listening");

                // Log once to stderr — matches the `jupyter` UX so the
                // operator can copy the token into curl / browser
                // tooling. Do NOT promote this to stdout or a file: the
                // token is only as strong as its handling. Suppressed
                // when `AURA_NODE_REQUIRE_AUTH` is off, because in that
                // mode there is no token to leak and the banner would
                // only add log noise.
                if require_auth {
                    eprintln!("[aura] API server listening on {url} — bearer token: {token}");
                } else {
                    eprintln!(
                        "[aura] API server listening on {url} — bearer auth disabled (set AURA_NODE_REQUIRE_AUTH=1 to enable)"
                    );
                }

                if port != API_PORT {
                    let _ = cmd_tx.try_send(UiCommand::ShowWarning(format!(
                        "Port {API_PORT} busy, API server using port {port}"
                    )));
                }

                let _ = cmd_tx.try_send(UiCommand::SetApiStatus {
                    url: Some(url.clone()),
                    active: true,
                });

                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        error!(error = %e, "API server error");
                    }
                });

                return Some(ApiServerHandle { url, token });
            }
            Err(e) => {
                debug!(port = port, error = %e, "Port unavailable, trying next");
            }
        }
    }

    warn!("Failed to start API server on any port");
    let _ = cmd_tx.try_send(UiCommand::SetApiStatus {
        url: None,
        active: false,
    });
    let _ = cmd_tx.try_send(UiCommand::ShowError(
        "API server failed to start - all ports busy".to_string(),
    ));
    None
}

/// Whether the embedded API server should enforce bearer-token auth.
///
/// Reads `AURA_NODE_REQUIRE_AUTH` — the same gate that controls the
/// aura-runtime router — and treats `1` / `true` (case-insensitive) as
/// "enable auth". Any other value, including unset, means auth is
/// disabled. Keeping the TUI API server aligned with `aura-runtime`
/// means local dev operators can toggle a single env var instead of
/// juggling two.
fn auth_required_from_env() -> bool {
    std::env::var("AURA_NODE_REQUIRE_AUTH").is_ok_and(|v| {
        let trimmed = v.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

/// Generate a random 32-byte hex token (~256 bits of entropy).
///
/// Uses two `uuid::Uuid::new_v4()` values concatenated so we don't have
/// to pull in `rand` just for this — `uuid` is already a workspace
/// dependency and v4 UUIDs are cryptographically random on every
/// supported platform.
fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

/// Axum middleware enforcing a constant-time bearer-token check.
///
/// `/health` is not behind this layer (liveness / readiness probes
/// should remain anonymous). Every other route requires the
/// `Authorization: Bearer <token>` header. Returns `401 UNAUTHORIZED`
/// on missing, malformed, or wrong token — distinguishing these cases
/// would leak whether a particular token-length probe had succeeded.
///
/// The actual parsing and constant-time compare live in
/// [`aura_runtime::auth::check_bearer`], shared with the aura-runtime HTTP
/// router so the two servers can't drift on edge cases (empty secrets,
/// prefix-length probing, etc.).
async fn require_bearer_mw(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    match check_bearer(request.headers(), &state.expected_token) {
        Ok(_) => next.run(request).await,
        Err(status) => status.into_response(),
    }
}

/// Health check endpoint handler.
async fn api_health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

#[derive(serde::Deserialize)]
struct ListFilesQuery {
    #[serde(default = "default_files_path")]
    path: String,
    #[serde(default = "default_files_depth")]
    depth: usize,
}

fn default_files_path() -> String {
    ".".into()
}
fn default_files_depth() -> usize {
    3
}

/// Wire shape for `/api/files` (TUI variant).
///
/// Paths are reported as absolute strings (matching the pre-consolidation
/// behaviour) because the TUI workspace panel resolves them directly.
/// The `aura-runtime` HTTP router uses a different DTO with workspace-
/// relative paths — the two JSON contracts are deliberately separate
/// and each caller owns its own mapping away from
/// [`aura_runtime::files_api::WalkedEntry`].
#[derive(serde::Serialize)]
struct DirEntry {
    name: String,
    path: String,
    is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<Self>>,
}

fn to_dir_entries(entries: Vec<WalkedEntry>) -> Vec<DirEntry> {
    entries
        .into_iter()
        .map(|e| DirEntry {
            name: e.name,
            path: e.abs_path.to_string_lossy().into_owned(),
            is_dir: e.is_dir,
            children: e.children.map(to_dir_entries),
        })
        .collect()
}

/// `GET /api/files?path=...&depth=...`
///
/// Lists directory contents recursively, returning a tree of
/// [`DirEntry`] objects. Depth is clamped to
/// [`aura_runtime::files_api::MAX_WALK_DEPTH`].
async fn api_list_files_handler(
    State(state): State<ApiState>,
    Query(query): Query<ListFilesQuery>,
) -> Response {
    let target = match state.sandbox.resolve_existing(&query.path) {
        Ok(p) => p,
        Err(e) => return sandbox_error_response(&e),
    };

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": "path not found" })),
            )
                .into_response();
        }
    };
    if !meta.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "path is not a directory" })),
        )
            .into_response();
    }

    let max_depth = query.depth.min(MAX_WALK_DEPTH);
    let walked = files_api::walk_directory(&target, Some(state.sandbox.root()), max_depth).await;
    let entries = to_dir_entries(walked);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "entries": entries })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct ReadFileQuery {
    path: String,
}

/// `GET /api/read-file?path=...`
///
/// Reads a file and returns its text content, capped at
/// [`aura_runtime::files_api::MAX_READ_BYTES`]. Files whose canonical path
/// is outside the sandbox root are refused with `403 Forbidden`; files
/// exceeding the cap return `413 Payload Too Large`.
async fn api_read_file_handler(
    State(state): State<ApiState>,
    Query(query): Query<ReadFileQuery>,
) -> Response {
    let target = match state.sandbox.resolve_existing(&query.path) {
        Ok(p) => p,
        Err(e) => return sandbox_error_response(&e),
    };

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": "path not found" })),
            )
                .into_response();
        }
    };
    if !meta.is_file() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "path is not a file" })),
        )
            .into_response();
    }

    match files_api::read_file_capped(&target, MAX_READ_BYTES).await {
        Ok(ReadOutcome::Ok { bytes }) => {
            let content = String::from_utf8_lossy(&bytes).into_owned();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "content": content,
                    "path": target.to_string_lossy(),
                    "bytes": bytes.len(),
                })),
            )
                .into_response()
        }
        Ok(ReadOutcome::TooLarge { max_bytes }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("file exceeds {max_bytes}-byte read cap"),
                "max_bytes": max_bytes,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": aura_auth::redact_error(e.to_string()),
            })),
        )
            .into_response(),
    }
}

/// Map [`aura_tools::ToolError`] variants onto HTTP responses.
///
/// `SandboxViolation` is `403 Forbidden` because the caller asked for
/// something they aren't allowed to see; `PathNotFound` is `404`;
/// anything else (I/O, permission) falls through as `400 Bad Request`.
fn sandbox_error_response(err: &aura_tools::ToolError) -> Response {
    use aura_tools::ToolError;
    match err {
        ToolError::SandboxViolation { .. } => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "path escapes workspace" })),
        )
            .into_response(),
        ToolError::PathNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "path not found" })),
        )
            .into_response(),
        other => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": other.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    /// Build a router identical to the one [`start_api_server`] mounts,
    /// but without the TCP bind / spawn — so tests can drive it with
    /// `oneshot`. Returns `(router, token, tempdir)` where the
    /// `tempdir` is the sandbox root and must be kept alive for the
    /// duration of the test.
    fn test_app() -> (axum::Router, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(tmp.path()).unwrap();
        let token = generate_token();
        let state = ApiState {
            expected_token: Arc::new(token.clone()),
            sandbox: Arc::new(sandbox),
        };
        let public = Router::new().route("/health", get(api_health_handler));
        let protected = Router::new()
            .route("/api/files", get(api_list_files_handler))
            .route("/api/read-file", get(api_read_file_handler))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_bearer_mw,
            ));
        let app = Router::new()
            .merge(public)
            .merge(protected)
            .with_state(state);
        (app, token, tmp)
    }

    fn urlencode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            let safe = b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':' | b'\\');
            if safe {
                out.push(b as char);
            } else {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
        out
    }

    #[tokio::test]
    async fn health_is_anonymous() {
        let (app, _token, _tmp) = test_app();
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn read_file_without_token_is_unauthorized() {
        let (app, _token, tmp) = test_app();
        std::fs::write(tmp.path().join("ok.txt"), "hi").unwrap();
        let uri = format!(
            "/api/read-file?path={}",
            urlencode(&tmp.path().join("ok.txt").to_string_lossy())
        );
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn read_file_rejects_path_traversal() {
        let (app, token, tmp) = test_app();
        // Secret lives one level *above* the sandbox root so any
        // traversal attempt that resolves to it must fail.
        let parent = tmp.path().parent().unwrap();
        let secret = parent.join(format!(
            "aura-api-server-secret-{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&secret, "top-secret").unwrap();

        let uri = format!(
            "/api/read-file?path={}",
            urlencode(&secret.to_string_lossy())
        );
        let req = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Cleanup before asserting so the file doesn't leak when the
        // test is re-run.
        let _ = std::fs::remove_file(&secret);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn read_file_returns_sandboxed_file() {
        let (app, token, tmp) = test_app();
        let path = tmp.path().join("hello.txt");
        std::fs::write(&path, "hello, world").unwrap();

        let uri = format!("/api/read-file?path={}", urlencode(&path.to_string_lossy()));
        let req = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["content"], "hello, world");
    }

    #[tokio::test]
    async fn read_file_caps_oversize() {
        let (app, token, tmp) = test_app();
        let path = tmp.path().join("big.bin");
        let payload = vec![b'A'; 5 * 1024 * 1024 + 1];
        std::fs::write(&path, &payload).unwrap();

        let uri = format!("/api/read-file?path={}", urlencode(&path.to_string_lossy()));
        let req = Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // The constant-time bearer compare now lives in
    // `aura_runtime::auth::check_bearer` and is exercised by that crate's
    // test suite. No duplicated test kept here — see
    // `crates/aura-runtime/src/auth.rs::tests` for coverage.
}
