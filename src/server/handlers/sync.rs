//! POST /api/v1/sync — manual incremental sync (watcher-driven polls use the
//! same `AccountSync::poll_once` path).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::json;

use crate::server::error::ApiResult;
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;

    let (new_count, revoke_count) = tokio::task::spawn_blocking({
        let sync = account.sync.clone();
        move || sync.lock().poll_once()
    })
    .await
    .map_err(|e| crate::server::error::ApiError::internal(format!("sync task failed: {e}")))?
    .map_err(crate::server::error::ApiError::from)?;

    Ok(Json(json!({
        "success": true,
        "newMessages": new_count,
        "revokeMessages": revoke_count,
    })))
}
