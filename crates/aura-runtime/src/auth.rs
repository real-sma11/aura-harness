//! Shared bearer-token authentication helper.
//!
//! Both the `aura-runtime` HTTP gateway (`crates/aura-runtime/src/gateway/auth_mw.rs`)
//! and the embedded TUI API server (`src/api_server.rs`) need a
//! constant-time `Authorization: Bearer <token>` check against a
//! configured secret. Previously each site had its own copy — two
//! different `constant_time_eq` implementations and two different
//! header-extraction helpers that had drifted on edge cases (e.g. only
//! one rejected empty secrets).
//!
//! This module is the single canonical implementation. The two
//! middleware layers still live where they are (they each bind to
//! their own `State<_>` shape) but the actual bytes-in, verdict-out
//! step delegates here.
//!
//! # Contract
//!
//! [`check_bearer`] returns `Err(StatusCode::UNAUTHORIZED)` when any of
//! the following hold:
//!
//! * the `Authorization` header is missing or not valid UTF-8,
//! * the value does not start with the literal prefix `"Bearer "`,
//! * the token after the prefix is empty (after trimming),
//! * the expected secret is empty (a misconfigured server must not
//!   authenticate with a random value),
//! * the presented token does not equal the expected secret under a
//!   constant-time compare.
//!
//! On success the validated token is returned so downstream code can
//! attach it to request extensions or log a principal. The comparison
//! is constant-time so a network-adjacent attacker cannot probe the
//! secret byte-by-byte via response timing.

use axum::http::{HeaderMap, StatusCode};

/// Extract and verify an `Authorization: Bearer <token>` header against
/// `expected` in constant time.
///
/// See the module docs for the full rejection contract. On success the
/// presented (and validated) token is returned.
pub fn check_bearer(headers: &HeaderMap, expected: &str) -> Result<String, StatusCode> {
    // Refuse to auth when the server has no secret loaded — otherwise a
    // caller who submits `Bearer ""` could match and succeed. Keeping
    // this as a separate early return localises the "empty secret"
    // misconfiguration bug report.
    if expected.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_str()
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    let token = value
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?
        .trim();

    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(token.to_string())
}

/// Verify a bearer header against the node token **or** an optional
/// fallback secret (Swarm TEE R2 integration fix).
///
/// The fallback exists for the swarm control plane: the scheduler
/// injects `AURA_SWARM_INTERNAL_TOKEN` into confidential pods, and the
/// gateway-side cron service presents that same token as its bearer
/// when firing `POST /v1/processes/:id/trigger` into the pod. The
/// platform token is therefore a valid principal alongside the
/// per-node auth token.
///
/// Both comparisons go through [`check_bearer`], so each is
/// constant-time and an empty/unset fallback can never authenticate.
pub fn check_bearer_any(
    headers: &HeaderMap,
    expected: &str,
    fallback: Option<&str>,
) -> Result<String, StatusCode> {
    match check_bearer(headers, expected) {
        Ok(token) => Ok(token),
        Err(status) => match fallback {
            Some(secret) => check_bearer(headers, secret).map_err(|_| status),
            None => Err(status),
        },
    }
}

/// Constant-time byte-slice compare.
///
/// Folds the length difference into the accumulator so timing cannot
/// leak whether lengths matched. We collapse the length xor to a
/// single bit (`len_xor != 0`) because we only care about the equality
/// verdict and the `usize -> u8` truncation would otherwise trigger
/// `clippy::cast_possible_truncation`. A timing-observer still learns
/// only "unequal" — not which byte or bit differed.
///
/// An inline implementation is used instead of pulling in the `subtle`
/// crate; both servers need exactly this much, and it's easier to
/// audit than a new transitive dependency.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_xor = a.len() ^ b.len();
    let n = a.len().min(b.len());
    let mut acc: u8 = u8::from(len_xor != 0);
    for i in 0..n {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const EXPECTED: &str = "expected-token";

    #[test]
    fn rejects_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_bearer(&headers, EXPECTED),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Basic abc"),
        );
        assert_eq!(
            check_bearer(&headers, EXPECTED),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_empty_presented_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer "),
        );
        assert_eq!(
            check_bearer(&headers, EXPECTED),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_wrong_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer not-the-right-value"),
        );
        assert_eq!(
            check_bearer(&headers, EXPECTED),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_token_with_different_length() {
        // A shorter token that is a prefix of EXPECTED used to let an
        // attacker probe byte-by-byte via timing; the constant-time
        // path must still reject it with 401 and no length leak.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected"),
        );
        assert_eq!(
            check_bearer(&headers, EXPECTED),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_when_expected_is_empty() {
        // A misconfigured server that forgot to load a secret must
        // not accept `Bearer <anything>` — otherwise the auth layer
        // would be a no-op.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer whatever"),
        );
        assert_eq!(check_bearer(&headers, ""), Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn accepts_matching_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected-token"),
        );
        assert_eq!(check_bearer(&headers, EXPECTED), Ok(EXPECTED.to_string()));
    }

    #[test]
    fn bearer_any_accepts_primary_and_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer swarm-internal-token"),
        );

        // Fallback (the swarm internal token) authenticates...
        assert_eq!(
            check_bearer_any(&headers, EXPECTED, Some("swarm-internal-token")),
            Ok("swarm-internal-token".to_string())
        );
        // ...and the primary node token still works with a fallback set.
        let mut node_headers = HeaderMap::new();
        node_headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer expected-token"),
        );
        assert_eq!(
            check_bearer_any(&node_headers, EXPECTED, Some("swarm-internal-token")),
            Ok(EXPECTED.to_string())
        );
    }

    #[test]
    fn bearer_any_rejects_wrong_token_and_empty_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer not-a-valid-secret"),
        );
        assert_eq!(
            check_bearer_any(&headers, EXPECTED, Some("swarm-internal-token")),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            check_bearer_any(&headers, EXPECTED, None),
            Err(StatusCode::UNAUTHORIZED)
        );

        // An empty fallback secret must never authenticate anything
        // (mirrors the empty-expected rule of `check_bearer`).
        let mut empty_headers = HeaderMap::new();
        empty_headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer "),
        );
        assert_eq!(
            check_bearer_any(&empty_headers, EXPECTED, Some("")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
