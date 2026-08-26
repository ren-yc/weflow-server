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
    // Header
    if let Some(h) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(v) = h.to_str()
            && let Some(token) = v.strip_prefix("Bearer ")
                && constant_time_eq(token.as_bytes(), state.token.as_bytes()) {
                    return true;
                }
    // Query / body
    if let Some(t) = params.get("access_token")
        && constant_time_eq(t.as_bytes(), state.token.as_bytes()) {
            return true;
        }
    false
}

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
pub mod sns;