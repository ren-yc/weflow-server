//! GET/POST /api/v1/sessions — conversation list (+ ChatLab shape).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{Page, SessionChatlab, SessionNative, SessionsChatlab, SessionsNative};
use crate::server::error::ApiResult;
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<axum::response::Response> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;
    let keyword = params.get("keyword").filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
    let limit = crate::server::parse_limit(&params, "limit", 100, 10000);
    // `cursor` 是 `page.nextCursor` 的回传入参；解析不了就退回 `offset`，
    // 与其它参数一样「坏值退化为默认而不是报错」。
    let offset = params
        .get("cursor")
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or_else(|| crate::server::parse_offset(&params, "offset"));
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
    // Offset paging (qqflow-server parity): keyword filter + stable sort happen
    // BEFORE the page slice, so `offset` walks the filtered, sorted set. An
    // `offset` past the end yields an empty page (count=0, success=true); both
    // the native and chatlab shapes below page from the same slice.
    //
    // `total` 是切片前的长度，也就是「匹配到的总数」：ChatLab 把**没有 page 块**
    // 的响应读作「这就是完整一页」，所以截断必须显式告知，否则第 limit 条之后的
    // 会话会被静默丢掉。
    let total = sessions.len();
    let sessions: Vec<&crate::store::Session> =
        sessions.into_iter().skip(offset).take(limit).collect();
    let next_offset = offset + sessions.len();
    let has_more = next_offset < total;

    if chatlab {
        let items: Vec<SessionChatlab> = sessions
            .iter()
            .map(|s| SessionChatlab {
                id: s.username.clone(),
                last_message_at: s.last_timestamp,
                message_count: store.conv_count(&s.username),
                name: store.session_display(&s.username),
                platform: "wechat".to_string(),
                r#type: if s.kind == crate::store::SessionKind::Group {
                    "group".to_string()
                } else {
                    "private".to_string()
                },
            })
            .collect();
        // `count` 是**本页条数**（与原生面同义）；「还有没有更多」由 page 表达。
        // 排空时 nextCursor 为 null，调用方据此停止翻页。
        let body = SessionsChatlab {
            count: sessions.len(),
            page: Page {
                has_more,
                next_cursor: has_more.then(|| next_offset.to_string()),
            },
            sessions: items,
        };
        return Ok(axum::response::IntoResponse::into_response(Json(body)));
    }

    let items: Vec<SessionNative> = sessions
        .iter()
        .map(|s| SessionNative {
            display_name: store.session_display(&s.username),
            last_timestamp: s.last_timestamp,
            message_count: store.conv_count(&s.username),
            session_type: s.kind.as_str().to_string(),
            summary: s.summary.clone(),
            r#type: s.kind as i64,
            unread_count: s.unread_count,
            username: s.username.clone(),
        })
        .collect();
    let body = SessionsNative { count: items.len(), sessions: items, success: true };
    Ok(axum::response::IntoResponse::into_response(Json(body)))
}
