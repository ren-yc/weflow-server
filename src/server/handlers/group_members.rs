//! GET /api/v1/group-members — 群成员（**名册 ∪ 发言人**）＋ 可选的发言计数。
//!
//! 成员集合为什么要并名册：只列发言人会让「群里有谁」这个问题的答案取决于谁最近说过话 ——
//! 潜水成员永远不出现，而他们恰恰是「这个群还有谁」的主要部分。代价是出现 messageCount 为 0
//! 的成员，这是接受的（名册里没有发言记录的人本来就没有计数）。

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
    // 鉴权只看查询串：POST body 不是鉴权通道。
    require_auth(&state, &query, &headers)?;
    let account = ready_account(&state, &params)?;
    let chatroom = params
        .get("chatroomId")
        .or_else(|| params.get("talker"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("chatroomId (or talker) is required"))?
        .clone();
    let with_counts = crate::server::flex_bool(&params, "includeMessageCounts");

    let store = account.store.read();

    // 发言人计数（本页无关：这是整个会话的计数）。
    let mut counts: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    if let Some(conv) = store.convs.get(&chatroom) {
        for m in conv.iter() {
            if !m.sender_username.is_empty() {
                *counts.entry(m.sender_username.as_str()).or_insert(0) += 1;
            }
        }
    }
    // 名册并进来；发言过但不在名册里的（退群、名册缺失）照旧保留 —— 两个方向的差集都要。
    let mut ids: Vec<&str> = counts.keys().copied().collect();
    if let Some(roster) = store.chatroom_roster.get(&chatroom) {
        for id in roster {
            if !id.is_empty() && !counts.contains_key(id.as_str()) {
                ids.push(id.as_str());
            }
        }
    }

    let mut members: Vec<GroupMember> = ids
        .into_iter()
        .map(|wxid| {
            let c = store.contacts.get(wxid);
            GroupMember {
                alias: c.and_then(|c| c.alias.clone()).unwrap_or_default(),
                avatar_url: c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
                // 名册里的潜水成员可能连联系人档案都没有：那时回落 uid，而不是给空串 ——
                // 空串会让下游把每一行都显示成一样的空白。
                display_name: store.sender_display(Some(&chatroom), wxid, wxid),
                group_nickname: store.group_card(Some(&chatroom), wxid),
                is_friend: c.map(|c| c.kind == crate::store::SessionKind::Private).unwrap_or(false),
                // 群主来自 contact.db::chat_room.owner（真库实测 114/114 非空，且都能在已读的
                // contact 表里解析出来）。取不到时是 false —— 与「不是群主」在响应上无法区分，
                // 但另一种选择（整条不发）会让字段缺失，对下游更难处理。
                is_owner: store.chatroom_owner.get(&chatroom).is_some_and(|o| o == wxid),
                message_count: if with_counts { counts.get(wxid).copied().unwrap_or(0) } else { 0 },
                nickname: c.and_then(|c| c.nickname.clone()).unwrap_or_default(),
                remark: c.and_then(|c| c.remark.clone()).unwrap_or_default(),
                wxid: wxid.to_string(),
            }
        })
        .collect();

    // 排序**必须稳定**：只按计数排时，一大批计数为 0 的潜水成员的相对顺序取决于哈希表的遍历
    // 顺序，同一个群两次请求的顺序就可能不同 —— 下游按它做 diff 时会看到满屏假变化。
    members.sort_by(|a, b| {
        b.message_count
            .cmp(&a.message_count)
            .then_with(|| a.wxid.cmp(&b.wxid))
    });

    Ok(Json(GroupMembers {
        chatroom_id: chatroom,
        count: members.len(),
        // 名册与消息都在内存索引里：这个请求既不读盘、也不触发同步（同步走 /api/v1/sync）。
        from_cache: false,
        members,
        success: true,
        updated_at: store.index_built_at_ms,
    }))
}
