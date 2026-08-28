//! Health check (unauthenticated). Mirror of qqflow-server: reports global
//! readiness plus the per-account status list so clients can observe
//! indexing/ready/error without polling the registration endpoint.
//!
//! The list also carries accounts found by the startup scan but not yet
//! registered (`awaiting_key`); they never affect `status` — see
//! `AppState::account_views`.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use crate::server::AppState;

pub async fn handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let (accounts, all_ready) = state.account_views();
    Json(json!({
        "status": if all_ready { "ok" } else { "starting" },
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": accounts,
    }))
}
