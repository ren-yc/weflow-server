//! GET/POST /api/v1/sessions — conversation list (+ ChatLab shape).

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
    let keyword = params.get("keyword").filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
    let limit = crate::server::parse_limit(&params, "limit", 100, 10000);
    let chatlab = crate::server::flex_bool(&params, "chatlab")
        || params.get("format").map(|f| f.eq_ignore_ascii_case("chatlab")).unwrap_or(false);

    let store = account.store.read();
    let mut sessions: Vec<&crate::store::Session> = store.sessions.values().collect();
    if let Some(kw) = &keyword {
        sessions.retain(|s| {
            s.username.to_lowercase().contains(kw)
                || store.session_display(&s.username).to_lowercase().contains(kw)
        });
    }
    sessions.sort_by(|a, b| b.last_timestamp.cmp(&a.last_timestamp).then(a.username.cmp(&b.username)));
    sessions.truncate(limit);

    if chatlab {
        let items: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.username,
                    "name": store.session_display(&s.username),
                    "platform": "wechat",
                    "type": if s.kind == crate::store::SessionKind::Group { "group" } else { "private" },
                    "messageCount": store.conv_count(&s.username),
                    "lastMessageAt": s.last_timestamp,
                })
            })
            .collect();
        return Ok(Json(json!({ "sessions": items })));
    }

    let items: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "username": s.username,
                "displayName": store.session_display(&s.username),
                "type": s.kind as i64,
                "sessionType": s.kind.as_str(),
                "lastTimestamp": s.last_timestamp,
                "unreadCount": s.unread_count,
                "messageCount": store.conv_count(&s.username),
                "summary": s.summary,
            })
        })
        .collect();
    Ok(Json(json!({ "success": true, "count": items.len(), "sessions": items })))
}