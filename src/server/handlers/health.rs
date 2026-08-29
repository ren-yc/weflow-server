//! Health check (unauthenticated) — `GET|POST /health` and `/api/v1/health`.
//!
//! Unauthenticated, so the response is deliberately scalar: `status` for
//! readiness, `version`, and a single `account` phase. It must NOT list the
//! accounts — the startup scan seeds one entry per `xwechat_files` profile
//! directory found on this machine, so the array (and even its length) told any
//! unauthenticated caller which accounts exist here and how far along each one
//! is. Account identities, message counts, database paths and error details are
//! served by the token-protected `GET /api/v1/accounts` instead.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use crate::server::AppState;

pub async fn handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let (account, ready) = state.account_phase();
    Json(json!({
        "status": if ready { "ok" } else { "starting" },
        "version": env!("CARGO_PKG_VERSION"),
        "account": account,
    }))
}
