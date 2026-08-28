//! GET /api/v1/sessions/:id/messages — ChatLab Pull protocol with a `sync`
//! pagination block (since/end/limit/offset → sync{hasMore,nextSince,...}).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{require_auth, ready_account};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_auth(&state, &query, &headers)?;
    let account = ready_account(&state, &query)?;
    let since = query.get("since").and_then(|s| s.parse::<i64>().ok());
    let end = query.get("end").and_then(|s| s.parse::<i64>().ok());
    let limit = crate::server::parse_limit(&query, "limit", 5000, 5000);
    let offset = crate::server::parse_offset(&query, "offset");

    let store = account.store.read();
    let conv = store
        .convs
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("session '{id}' not found")))?;

    // Upper bound of this pull, echoed as `watermark`: a client that has
    // drained the session resumes from here. It is a TIME BOUND, not the
    // newest message's timestamp, so an idle session still reports progress.
    let watermark = end.unwrap_or_else(|| chrono::Utc::now().timestamp());

    // Chronological messages within (since, end] — `since` is EXCLUSIVE, so a
    // client resuming with `nextSince` never re-fetches the boundary second
    // and can neither loop nor see duplicates (qqflow-server parity).
    let mut ascending: Vec<&crate::store::MessageRecord> = conv
        .iter()
        .filter(|m| since.is_none_or(|s| m.create_time > s) && end.is_none_or(|e| m.create_time <= e))
        .collect();
    ascending.sort_by(|a, b| {
        (a.create_time, a.sort_seq, a.local_id).cmp(&(b.create_time, b.sort_seq, b.local_id))
    });

    let total = ascending.len();
    // Page from `offset`, extending to the end of the last second's ts group:
    // pages never split a second, which is what lets `nextSince` (the page's
    // last timestamp) advance without dropping the rest of that second.
    let start = offset.min(total);
    let mut page_end = start;
    let mut prev_ts = None;
    while page_end < total {
        let ts = ascending[page_end].create_time;
        if prev_ts.is_some_and(|p| p != ts) && page_end - start >= limit {
            break;
        }
        prev_ts = Some(ts);
        page_end += 1;
    }
    let page: Vec<&crate::store::MessageRecord> = ascending[start..page_end].to_vec();
    let has_more = page_end < total;
    // The page's OWN last timestamp. Using the whole set's maximum here would
    // tell the client to resume past everything it has not seen yet.
    let next_since = page.last().map(|m| m.create_time).unwrap_or(since.unwrap_or(0));

    // members = senders in this page (dedup)
    let mut seen = std::collections::HashSet::new();
    let members: Vec<serde_json::Value> = page
        .iter()
        .filter(|m| !m.sender_username.is_empty() && seen.insert(m.sender_username.as_str()))
        .map(|m| {
            let c = store.contacts.get(&m.sender_username);
            json!({
                "platformId": m.sender_username,
                "accountName": m.sender_name,
                "groupNickname": c.and_then(|c| c.remark.clone()).unwrap_or_default(),
                "avatar": c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
            })
        })
        .collect();

    let messages: Vec<serde_json::Value> = page
        .iter()
        .map(|m| {
            json!({
                "sender": m.sender_username,
                "accountName": m.sender_name,
                "timestamp": m.create_time,
                "type": chatlab_type(m.local_type),
                "content": m.parsed.display,
                "platformMessageId": m.server_id.to_string(),
                "replyToMessageId": m.parsed.reply_to,
            })
        })
        .collect();

    Ok(Json(json!({
        "chatlab": { "version": "0.0.2", "exportedAt": chrono::Utc::now().timestamp(), "generator": "weflow-server" },
        "meta": {
            "name": store.session_display(&id),
            "platform": "wechat",
            "type": if id.ends_with("@chatroom") { "group" } else { "private" },
            "groupId": id,
            "ownerId": store.my_wxid,
        },
        "members": members,
        "messages": messages,
        "sync": {
            "hasMore": has_more,
            "nextSince": if has_more { next_since } else { watermark },
            // Both cursors are meant to be echoed back verbatim, so they must
            // not skip the same rows twice. `nextSince` is exclusive and the
            // page ends on a complete ts group, so re-filtering with it drops
            // exactly the rows already served — leaving the next unseen row at
            // offset 0. `nextOffset` therefore only carries weight in the
            // degenerate case where the timestamp could not advance at all.
            "nextOffset": if has_more && next_since <= since.unwrap_or(i64::MIN) {
                start.saturating_add(page.len())
            } else {
                0
            },
            "watermark": watermark,
        },
    })))
}

fn chatlab_type(t: i64) -> i64 {
    match t {
        1 => 0,
        3 => 1,
        34 => 2,
        43 => 3,
        49 => 4,
        47 => 5,
        50 => 6,
        48 => 7,
        10000 => 80,
        10002 => 81,
        _ => 99,
    }
}
