//! Health check (unauthenticated).

use axum::Json;
use serde_json::json;

pub async fn handler() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}