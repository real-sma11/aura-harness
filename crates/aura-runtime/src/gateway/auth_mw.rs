//! Router authentication middleware.
//!
//! Centralised bearer-token enforcement for mutating / privileged
//! routes so they all share the exact same extraction rules and
//! failure shape. Added in Wave 5 (T1.4) to close the "read token,
//! ignore value" pattern that existed on `/stream/automaton/:id`.
//!
//! The canonical constant-time bearer check lives in
//! [`crate::auth::check_bearer`] and is shared with the embedded
//! TUI API server (`src/api_server.rs`). This file owns only the
//! axum-flavoured middleware wrapper plus the `BearerToken`
//! extension newtype; the parsing, constant-time compare, and
//! rejection contract are inherited unchanged.

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::RouterState;
use crate::auth::check_bearer_any;

/// Strongly-typed Bearer token extracted by the auth middleware.
///
/// Inserted into request extensions by [`require_bearer_mw`] so downstream
/// handlers can read the validated token without having to re-parse the
/// `Authorization` header. Wrapped in a newtype so it can't be confused
/// with any other `String` extension.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by future handlers that want to surface the
                    // authenticated principal
pub(crate) struct BearerToken(pub String);

/// Axum middleware enforcing that every request presents the expected
/// Bearer token in its `Authorization` header.
///
/// The expected token is pulled from [`RouterState::config`], which was
/// populated by `Node::run` from `AURA_NODE_AUTH_TOKEN`, a persisted
/// `$data_dir/auth_token` file, or a freshly-minted per-launch random
/// value. When the node runs as a swarm pod, the platform internal
/// token (`AURA_SWARM_INTERNAL_TOKEN`, injected by the scheduler) is
/// accepted as an additional valid bearer — that is what the swarm
/// cron service presents when firing `POST /v1/processes/:id/trigger`
/// (Swarm TEE R2 integration fix). Both checks are constant-time via
/// [`crate::auth::check_bearer_any`].
///
/// Returns `401 UNAUTHORIZED` when neither secret matches. On success
/// the validated token is cloned into the request extensions as a
/// [`BearerToken`] before the request is forwarded to `next`.
pub(crate) async fn require_bearer_mw(
    State(state): State<RouterState>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = match check_bearer_any(
        request.headers(),
        &state.config.auth_token,
        state.config.swarm_internal_token.as_deref(),
    ) {
        Ok(t) => t,
        Err(status) => return status.into_response(),
    };
    request.extensions_mut().insert(BearerToken(token));
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};

    // The parsing / constant-time-compare contract is exhaustively
    // exercised in `crate::auth::tests`. These tests only verify the
    // thin axum wrapper: that the middleware delegates to
    // `check_bearer_any` and surfaces its verdict as a `StatusCode`.

    #[test]
    fn rejects_missing_header_via_shared_check() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_bearer_any(&headers, "expected-token", None),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn accepts_matching_bearer_via_shared_check() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected-token"),
        );
        assert_eq!(
            check_bearer_any(&headers, "expected-token", None),
            Ok("expected-token".to_string())
        );
    }

    /// The swarm platform internal token (`AURA_SWARM_INTERNAL_TOKEN`)
    /// must be accepted alongside the node token — it is the bearer the
    /// swarm cron service presents on trigger calls.
    #[test]
    fn accepts_swarm_internal_token_as_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer platform-internal"),
        );
        assert_eq!(
            check_bearer_any(&headers, "node-token", Some("platform-internal")),
            Ok("platform-internal".to_string())
        );
        // Without the fallback configured (local agent) it is rejected.
        assert_eq!(
            check_bearer_any(&headers, "node-token", None),
            Err(StatusCode::UNAUTHORIZED)
        );
    }
}
