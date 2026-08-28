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
pub mod sns;
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

// Auth lives in `server::auth`; re-exported so handlers keep importing it
// from `super::` unchanged.
pub use crate::server::auth::{authorized, require_auth};

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