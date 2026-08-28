//! Access-token authentication for the HTTP API.
//!
//! Five accepted transports, checked in this order (qqflow-server parity):
//!   1. `Authorization: Bearer <token>`
//!   2. `X-Api-Key: <token>`
//!   3. `?access_token=<token>` (query)
//!   4. `?token=<token>` (query)
//!   5. the same two keys inside a POST JSON body
//!
//! Query and body share one map: handlers merge them via
//! `handlers::extract_params` before calling in here, so 3-5 are one check.

use std::collections::HashMap;

use crate::server::error::{ApiError, ApiResult};
use crate::server::AppState;

/// True when the request carries the API token on any accepted transport.
pub fn authorized(
    state: &AppState,
    params: &HashMap<String, String>,
    headers: &axum::http::HeaderMap,
) -> bool {
    let expected = state.token.as_bytes();
    // Header: Authorization: Bearer <token>
    if let Some(h) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(v) = h.to_str()
        && let Some(token) = v.strip_prefix("Bearer ")
        && constant_time_eq(token.as_bytes(), expected)
    {
        return true;
    }
    // Header: X-Api-Key: <token>
    if let Some(h) = headers.get(axum::http::HeaderName::from_static("x-api-key"))
        && let Ok(token) = h.to_str()
        && constant_time_eq(token.as_bytes(), expected)
    {
        return true;
    }
    // Query / body: access_token | token
    if let Some(t) = params.get("access_token").or_else(|| params.get("token"))
        && constant_time_eq(t.as_bytes(), expected)
    {
        return true;
    }
    false
}

/// `authorized`, as a 401-returning guard.
pub fn require_auth(
    state: &AppState,
    params: &HashMap<String, String>,
    headers: &axum::http::HeaderMap,
) -> ApiResult<()> {
    if !authorized(state, params, headers) {
        return Err(ApiError::unauthorized("invalid or missing access token"));
    }
    Ok(())
}

/// Constant-time token comparison (qqflow-server style): the loop has no
/// early exit, so wall time reveals nothing beyond string length.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn constant_time_eq_behavior() {
        // equality
        assert!(constant_time_eq(b"token", b"token"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"\x00\x01", b"\x00\x01"));
        // single-byte difference anywhere → false
        assert!(!constant_time_eq(b"token", b"tokem"));
        assert!(!constant_time_eq(b"token", b"Token"));
        assert!(!constant_time_eq(b"a", b"b"));
        // length difference → false (never compares content)
        assert!(!constant_time_eq(b"token", b"token2"));
        assert!(!constant_time_eq(b"", b"x"));
        // binary-safe
        assert!(!constant_time_eq(&[0u8, 1, 2], &[0u8, 1, 3]));
    }

    fn test_state() -> Arc<AppState> {
        let cfg = crate::config::Config {
            host: "127.0.0.1".into(),
            port: 0,
            log: "info".into(),
            watch_debounce_ms: 10,
            watch_fallback_ms: 0,
            media_export_dir: std::path::PathBuf::from("target/test-tmp/auth"),
            base_url: None,
            data_dir: std::path::PathBuf::from("target/test-tmp/auth-data"),
            show_token: false,
        };
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Arc::new(AppState::new(cfg, "0123456789abcdef".to_string(), shutdown))
    }

    #[test]
    fn authorized_all_channels_constant_time() {
        let state = test_state();
        // empty params/headers → false
        assert!(!authorized(&state, &HashMap::new(), &axum::http::HeaderMap::new()));

        // Bearer header
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer 0123456789abcdef".parse().unwrap(),
        );
        assert!(authorized(&state, &HashMap::new(), &h));
        // wrong bearer → false
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer 0123456789abcde0".parse().unwrap(),
        );
        assert!(!authorized(&state, &HashMap::new(), &h));

        // X-Api-Key header
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderValue::from_static("0123456789abcdef"),
        );
        assert!(authorized(&state, &HashMap::new(), &h));
        // wrong X-Api-Key → false
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("x-api-key"),
            axum::http::HeaderValue::from_static("0123456789abcde0"),
        );
        assert!(!authorized(&state, &HashMap::new(), &h));

        // query params
        let mut p = HashMap::new();
        p.insert("access_token".into(), "0123456789abcdef".into());
        assert!(authorized(&state, &p, &axum::http::HeaderMap::new()));
        let mut p = HashMap::new();
        p.insert("token".into(), "0123456789abcdef".into());
        assert!(authorized(&state, &p, &axum::http::HeaderMap::new()));
        let mut p = HashMap::new();
        p.insert("token".into(), "wrong".into());
        assert!(!authorized(&state, &p, &axum::http::HeaderMap::new()));
    }

    #[test]
    fn require_auth_maps_to_401() {
        let state = test_state();
        let err = require_auth(&state, &HashMap::new(), &axum::http::HeaderMap::new())
            .expect_err("missing token must be rejected");
        assert_eq!(err.status, axum::http::StatusCode::UNAUTHORIZED);
        let mut p = HashMap::new();
        p.insert("access_token".into(), "0123456789abcdef".into());
        assert!(require_auth(&state, &p, &axum::http::HeaderMap::new()).is_ok());
    }
}
