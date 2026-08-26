//! Endpoint handlers (WeFlow-compatible shapes).

pub mod accounts;
pub mod chatlab_pull;
pub mod contacts;
pub mod group_members;
pub mod health;
pub mod media;
pub mod messages;
pub mod push_events;
pub mod sessions;
pub mod sync;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::{AccountHandle, AppState};

/// Shared extractor: merge GET query params and POST JSON body into one map
/// (body wins). `query` is the handler's `Query(query)` map.
pub fn extract_params<T: serde::de::DeserializeOwned + serde::Serialize>(
    query: &HashMap<String, String>,
    body: Option<Json<T>>,
) -> HashMap<String, String> {
    let mut out = query.clone();
    if let Some(body) = body
        && let Ok(value) = serde_json::to_value(body.0)
            && let Some(map) = value.as_object() {
                for (k, v) in map {
                    out.insert(
                        k.clone(),
                        match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    );
                }
            }
    out
}

pub fn authorized(state: &AppState, params: &HashMap<String, String>, headers: &axum::http::HeaderMap) -> bool {
    let expected = state.token.as_bytes();
    // Header: Authorization: Bearer <token>
    if let Some(h) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(v) = h.to_str()
            && let Some(token) = v.strip_prefix("Bearer ")
                && constant_time_eq(token.as_bytes(), expected) {
                    return true;
                }
    // Header: X-Api-Key: <token>
    if let Some(h) = headers.get(axum::http::HeaderName::from_static("x-api-key"))
        && let Ok(token) = h.to_str()
            && constant_time_eq(token.as_bytes(), expected) {
                return true;
            }
    // Query / body: access_token | token
    if let Some(t) = params.get("access_token").or_else(|| params.get("token"))
        && constant_time_eq(t.as_bytes(), expected) {
            return true;
        }
    false
}

/// Constant-time token comparison (qqflow-server style): the loop has no
/// early exit, so wall time reveals nothing beyond string length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

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

/// Require a registered, ready account; `wxid` from `wxid` param or the
/// default (first ready) account.
pub fn ready_account(
    state: &Arc<AppState>,
    params: &HashMap<String, String>,
) -> ApiResult<Arc<AccountHandle>> {
    let accounts = state.accounts.lock();
    let handle = match params.get("wxid") {
        Some(w) => accounts.get(w).cloned(),
        None => {
            // default: first ready account
            accounts.values().find(|a| a.status().is_ready()).cloned()
        }
    };
    let Some(handle) = handle else {
        return Err(ApiError::service_unavailable(
            "no account registered/ready; register via POST /api/v1/accounts",
        ));
    };
    if !handle.status().is_ready() {
        return Err(ApiError::service_unavailable("account not ready (indexing or error)"));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    fn test_state() -> Arc<crate::server::AppState> {
        let cfg = crate::config::Config {
            host: "127.0.0.1".into(),
            port: 0,
            log: "info".into(),
            watch_debounce_ms: 10,
            watch_fallback_ms: 0,
            media_export_dir: std::path::PathBuf::from("target/test-tmp/auth"),
            base_url: None,
            data_dir: std::path::PathBuf::from("target/test-tmp/auth-data"),
        };
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Arc::new(crate::server::AppState {
            cfg,
            token: "0123456789abcdef".to_string(),
            accounts: parking_lot::Mutex::new(HashMap::new()),
            shutdown,
        })
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
}

pub mod sns;