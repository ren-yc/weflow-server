//! GET/POST /api/v1/contacts — contact profiles from contact.db.

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

    let store = account.store.read();
    let mut contacts: Vec<&crate::store::Contact> = store.contacts.values().collect();
    if let Some(kw) = &keyword {
        contacts.retain(|c| {
            c.username.to_lowercase().contains(kw)
                || c.display_name().to_lowercase().contains(kw)
                || c.alias.as_deref().unwrap_or("").to_lowercase().contains(kw)
        });
    }
    contacts.sort_by_key(|a| a.display_name());
    contacts.truncate(limit);

    let items: Vec<serde_json::Value> = contacts
        .iter()
        .map(|c| {
            json!({
                "username": c.username,
                "displayName": c.display_name(),
                "remark": c.remark,
                "nickname": c.nickname,
                "alias": c.alias,
                "avatarUrl": c.avatar_url,
                "type": c.kind.as_str(),
            })
        })
        .collect();
    Ok(Json(json!({ "success": true, "count": items.len(), "contacts": items })))
}