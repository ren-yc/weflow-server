//! GET /api/v1/sessions/:id/messages — ChatLab Pull protocol with a `sync`
//! pagination block (since/end/limit/offset → sync{hasMore,nextSince,...}).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{ChatlabMember, PullEnvelope, PullMessage, PullSync};
use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{require_auth, ready_account};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Json<PullEnvelope>> {
    require_auth(&state, &query, &headers)?;
    let account = ready_account(&state, &query)?;
    // WeFlow (安装版) documents both as unix seconds; accepting "YYYYMMDD" too
    // is a superset that cannot change the meaning of a numeric cursor (an
    // 8-digit unix second is 1970-04-26, far below any real message), and it
    // keeps this face consistent with `/api/v1/messages`'s time bounds.
    let since = query.get("since").and_then(|s| crate::server::parse_time_bound(s));
    let end = query.get("end").and_then(|s| crate::server::parse_time_bound_end(s));
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

    // `groupNickname` is the sender's per-chatroom card, which lives in
    // `group_cards` — NOT the contact's 备注. The two are different things and
    // serving the remark here made `groupNickname` wrong for every group
    // member who had a remark but no card (and for every private chat).
    let chatroom = id.ends_with("@chatroom").then_some(id.as_str());

    // members = senders in this page (dedup)。字段取法与消息面共用一处（server::chatlab）：
    // 两处各写一遍时，「取联系人档案还是取消息里的昵称」这种分歧不会有任何东西变红。
    let mut seen = std::collections::HashSet::new();
    let members: Vec<ChatlabMember> = page
        .iter()
        .filter(|m| !m.sender_username.is_empty() && seen.insert(m.sender_username.as_str()))
        .map(|m| crate::server::chatlab::member_fields(&store, chatroom, &m.sender_username))
        .collect();

    let messages: Vec<PullMessage> = page
        .iter()
        .map(|m| {
            // 字段怎么填只有一处出处（`server::chatlab`）：三个面（原生面、消息面、拉取面）在
            // `replyToMessageId` 与 `media` 上同规 —— 无引用时省略该键，无媒体时省略整个 media。
            let f = crate::server::chatlab::message_fields(&store, chatroom, m);
            PullMessage {
                account_name: f.account_name,
                content: f.content,
                group_nickname: f.group_nickname,
                media: f.media,
                platform_message_id: f.platform_message_id,
                reply_to_message_id: f.reply_to_message_id,
                sender: f.sender,
                timestamp: f.timestamp,
                r#type: f.r#type,
            }
        })
        .collect();

    let body = PullEnvelope {
        chatlab: crate::server::chatlab::header(),
        members,
        messages,
        meta: crate::server::chatlab::meta(&store, &id, id.clone(), store.my_wxid.clone()),
        sync: PullSync {
            has_more,
            next_since: if has_more { next_since } else { watermark },
            // Both cursors are meant to be echoed back verbatim, so they must
            // not skip the same rows twice. `nextSince` is exclusive and the
            // page ends on a complete ts group, so re-filtering with it drops
            // exactly the rows already served — leaving the next unseen row at
            // offset 0. `nextOffset` therefore only carries weight in the
            // degenerate case where the timestamp could not advance at all.
            next_offset: if has_more && next_since <= since.unwrap_or(i64::MIN) {
                start.saturating_add(page.len())
            } else {
                0
            },
            watermark,
        },
    };
    Ok(Json(body))
}

