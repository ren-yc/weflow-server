//! Account registration (client-driven startup, qqflow-server style).
//!
//! Body: `{wxid, key?, keys?: {rel: hex}, img_code?, db_path?}`.
//! The key is validated deterministically against the account's session.db
//! (page-1 HMAC). On success the live acquisition + in-memory index
//! blocking task and a watcher task is spawned.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::AppState;

use super::require_auth;

/// Registration payload. Keys live in memory only and must be re-supplied
/// after a server restart.
#[derive(Clone, Deserialize, Default)]
pub struct AccountBody {
    pub wxid: Option<String>,
    pub key: Option<String>,
    pub keys: Option<std::collections::HashMap<String, String>>,
    pub img_code: Option<String>,
    /// direct image keys (preferred over img_code)
    pub img_aes_key: Option<String>,
    pub img_xor_key: Option<String>,
    pub db_path: Option<String>,
    // (persist option removed: keys stay in memory only and
    //  must be re-supplied on restart)


}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    // Keep the raw JSON body for proper deserialization (nested `keys` map
    // is lost when flattening to string params).
    let raw_body: Option<serde_json::Value> = body.map(|j| j.0);
    let mut params = query.clone();
    if let Some(v) = &raw_body
        && let Some(map) = v.as_object()
    {
        for (k, val) in map {
            params.insert(
                k.clone(),
                match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
            );
        }
    }
    require_auth(&state, &params, &headers)?;

    let body: AccountBody = match raw_body {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))?,
        None => AccountBody {
            wxid: params.get("wxid").cloned(),
            key: params.get("key").cloned(),
            keys: None,
            img_code: params.get("img_code").cloned(),
            img_aes_key: params.get("img_aes_key").cloned(),
            img_xor_key: params.get("img_xor_key").cloned(),
            db_path: params.get("db_path").cloned(),

        },
    };
    // delegate to the shared registration path (spawns the async
    // build/watcher)
    let handle = crate::server::start_account(state.clone(), body).await?;

    Ok(Json(json!({
        "success": true,
        "wxid": handle.info.wxid,
        "status": "indexing",
        "db_storage": handle.info.db_storage.to_string_lossy(),
    })))
}