//! GET/POST /api/v1/group-members — members of a chatroom with optional
//! message counts (sender occurrence counts from the indexed conversation).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{GroupMember, GroupMembers};
use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<GroupMembers>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;
    let chatroom = params
        .get("chatroomId")
        .or_else(|| params.get("talker"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("chatroomId (or talker) is required"))?
        .clone();
    let with_counts = crate::server::flex_bool(&params, "includeMessageCounts")
        || crate::server::flex_bool(&params, "withCounts");
    let force = crate::server::flex_bool(&params, "forceRefresh");

    // forceRefresh: run an incremental sync first so the member universe and
    // message counts reflect the freshest database state (WeFlow contract).
    let mut refreshed = false;
    if force {
        let sync = account.sync.clone();
        let res = tokio::task::spawn_blocking(move || sync.lock().poll_once())
            .await
            .map_err(|e| crate::server::error::ApiError::internal(format!("sync task failed: {e}")))?;
        if res.is_ok() {
            refreshed = true;
        }
    }

    let store = account.store.read();
    // member universe: senders appearing in the conversation ∪ contacts of
    // this chatroom type; keep it deterministic
    let conv = store.convs.get(&chatroom);
    let mut member_map: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let Some(conv) = conv {
        for m in conv.iter() {
            if !m.sender_username.is_empty() {
                *member_map.entry(m.sender_username.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut members: Vec<GroupMember> = member_map
        .iter()
        .map(|(wxid, count)| {
            let c = store.contacts.get(wxid);
            let card = store
                .group_cards
                .get(&chatroom)
                .and_then(|cards| cards.get(wxid))
                .cloned()
                .unwrap_or_default();
            GroupMember {
                alias: c.and_then(|c| c.alias.clone()).unwrap_or_default(),
                avatar_url: c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
                display_name: store.sender_display(Some(&chatroom), wxid, wxid),
                group_nickname: card,
                is_friend: c.map(|c| c.kind == crate::store::SessionKind::Private).unwrap_or(false),
                is_owner: false,
                message_count: if with_counts { *count } else { 0 },
                nickname: c.and_then(|c| c.nickname.clone()).unwrap_or_default(),
                remark: c.and_then(|c| c.remark.clone()).unwrap_or_default(),
                wxid: wxid.clone(),
            }
        })
        .collect();
    // 直接在 struct 上排，不再读 `Value` 的字符串键：键名写错不会报错，只会静默排错。
    members.sort_by_key(|m| std::cmp::Reverse(m.message_count));

    Ok(Json(GroupMembers {
        chatroom_id: chatroom,
        count: members.len(),
        members,
        refreshed,
        success: true,
    }))
}
