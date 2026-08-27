//! Health check (unauthenticated). Mirror of qqflow-server: reports global
//! readiness plus the per-account status list so clients can observe
//! indexing/ready/error without polling the registration endpoint.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use crate::server::{AccountStateView, AppState};

pub async fn handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let accounts: Vec<AccountStateView> = {
        let accs = state.accounts.lock();
        let mut views: Vec<_> = accs
            .values()
            .map(|h| AccountStateView {
                wxid: h.info.wxid.clone(),
                state: h.status(),
                message_count: h.store.read().total_messages(),
                error: h.error.lock().clone(),
            })
            .collect();
        views.sort_by(|a, b| a.wxid.cmp(&b.wxid));
        views
    };
    let all_ready = !accounts.is_empty() && accounts.iter().all(|a| a.state.is_ready());
    Json(json!({
        "status": if all_ready { "ok" } else { "starting" },
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": accounts,
    }))
}