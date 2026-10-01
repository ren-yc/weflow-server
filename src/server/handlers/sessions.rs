//! GET /api/v1/sessions — 会话列表（**只输出原生形状**）。
//!
//! ChatLab 形状走 `/chatlab/sessions`（见 `chatlab_sessions`）。两个面共用本函数的其余部分
//! （鉴权、筛选、稳定排序、切片），但**分页参数不共用**：老面只认 `offset`，ChatLab 形状认
//! `cursor`（同时接受 `offset`）。一刀切会让「换个面就该换参数」这件事静默失效。

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
    respond(&state, &query, &headers, body, false).await
}

/// `handler` 的本体，带一个「本面就是 ChatLab 形状」的开关。
///
/// `/chatlab/sessions` 用它并传 `true`：那个面**天生就是** ChatLab 形状，调用方不必知道还有
/// 另一种。两条路的其余部分（鉴权、筛选、稳定排序、切片）**逐字一致** —— 抄一份就会漂移，而漂移
/// 的后果是两个面在分页边界上表现不同。
pub(crate) async fn respond(
    state: &Arc<AppState>,
    query: &std::collections::HashMap<String, String>,
    headers: &HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
    force_chatlab: bool,
) -> ApiResult<axum::response::Response> {
    let params = extract_params(query, body);
    // 鉴权只看查询串：POST body 不是鉴权通道。
    require_auth(state, query, headers)?;
    let account = ready_account(state, &params)?;
    let keyword = params.get("keyword").filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
    let limit = crate::server::parse_limit(&params, "limit", 100, 10000);
    // 分页模型按面分开：ChatLab 形状认 `cursor`（`page.nextCursor` 的回传入参，解析不了就退回
    // `offset`，与其它参数一样「坏值退化为默认而不是报错」）；老面**只认 offset** —— 继续接受
    // `cursor` 会让「换个面就该换参数」这件事静默失效（调用方以为自己在翻页，其实一直拿第一页）。
    let offset = if force_chatlab {
        params
            .get("cursor")
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or_else(|| crate::server::parse_offset(&params, "offset"))
    } else {
        crate::server::parse_offset(&params, "offset")
    };
    let chatlab = force_chatlab;

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
                member_count: store
                    .chatroom_roster
                    .get(&s.username)
                    .map(|r| r.len()),
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
