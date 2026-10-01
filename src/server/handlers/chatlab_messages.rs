//! GET /chatlab/messages —— ChatLab 形状的消息面。
//!
//! 它是原「混合面」（/api/v1/messages?chatlab=1）的**新家**：老面不再输出 ChatLab 形状，而这里
//! 天生就是 ChatLab 形状 —— 调用方不必知道还有另一种。
//!
//! 与原生面的差异都在**参数与信封**上：cursor 翻页（同时接受 offset）、信封是
//! {talker,count,page,chatlab,meta,members,messages}、**没有 success**、消息**升序**。
//! 参数解析、筛选排序切片与导出任务收集与原生面**共用同一份实现**（messages 模块）——
//! 抄一份就会漂移，而漂移的后果是两个面对同一批参数给出不同的页。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{ChatlabMember, ChatlabMessage, ChatlabMessages, Page};
use crate::server::error::ApiResult;
use crate::server::handlers::messages::{
    collect_export_jobs, media_requested, page_slice, run_export_batch, MessageQuery,
};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<axum::response::Response> {
    let params = extract_params(&query, body);
    // 鉴权只看查询串：POST body 不是鉴权通道。本面也不接 body 参数（它是只读面）。
    require_auth(&state, &query, &headers)?;
    let account = ready_account(&state, &params)?;
    let q = MessageQuery::from_params_with_cursor(&params)?;
    let include_media = media_requested(&params);

    // store 的读 guard 不是 Send：所有碰索引的事都在这个 scope 内做完，出 scope 再 await。
    let (page, export_jobs, messages, members, meta, local_ids) = {
        let store = account.store.read();
        let page = page_slice(&store, &q)?;
        let jobs = if include_media {
            collect_export_jobs(&store, &page, &q.talker, &params)
        } else {
            Vec::new()
        };
        let conv = store
            .convs
            .get(&q.talker)
            .expect("page_slice 已经确认这个会话存在");
        // 切片是原生面的**降序**（最新在前），这里反转回时间顺序：ChatLab 的读者按正序合并，
        // 倒序会让他们以为最新一条排在最前面。
        let mut slice: Vec<&crate::store::MessageRecord> =
            page.indices.iter().map(|&i| &conv[i]).collect();
        slice.reverse();
        let chatroom = q.talker.ends_with("@chatroom").then_some(q.talker.as_str());
        // members 是**本页出现的发送者**，不是名册：这个面描述的是这一页，把名册并进来会让
        // members 与 messages 的关系在不同页上不一致（名册只见于群成员面）。
        let mut seen = std::collections::HashSet::new();
        let members: Vec<ChatlabMember> = slice
            .iter()
            .filter(|m| !m.sender_username.is_empty() && seen.insert(m.sender_username.as_str()))
            .map(|m| crate::server::chatlab::member_fields(&store, chatroom, &m.sender_username))
            .collect();
        let mut local_ids: Vec<i64> = Vec::with_capacity(slice.len());
        let messages: Vec<ChatlabMessage> = slice
            .iter()
            .map(|m| {
                // 字段怎么填只有一处出处（server::chatlab）；这里只剩这个面自己的信封。
                local_ids.push(m.local_id);
                let f = crate::server::chatlab::message_fields(&store, chatroom, m);
                ChatlabMessage {
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
        let (group_id, owner_id) = (q.talker.clone(), store.my_wxid.clone());
        let meta = crate::server::chatlab::meta(&store, &q.talker, group_id, owner_id);
        (page, jobs, messages, members, meta, local_ids)
    };

    // media=1：**真正执行导出**。旧混合面在收集任务之前就 return 了，因此 media=1 在 ChatLab
    // 形状上从未导出过 —— 于是「先触发导出、再取字节」这条两步走在那个面上并不成立。
    //
    // 回填规则：只有**确实写出了本地副本**、且名字**由内容摘要派生**的那些，fileName 才是可取
    // 句柄（外链与平台名都给不出跨会话唯一的句柄）。没落盘的照给元数据、不给句柄。
    let exported = run_export_batch(&state, &account, export_jobs).await;
    let mut messages = messages;
    for (idx, local_id) in local_ids.iter().enumerate() {
        let Some(res) = exported.get(local_id) else {
            continue;
        };
        if res.external_url.is_some() || !res.digest_named {
            continue;
        }
        if let Some(media) = messages[idx].media.as_mut() {
            media.file_name = res.file_name.clone();
        }
    }

    Ok(axum::response::IntoResponse::into_response(Json(ChatlabMessages {
        chatlab: crate::server::chatlab::header(),
        count: page.count(),
        members,
        messages,
        meta,
        page: Page {
            has_more: page.has_more,
            next_cursor: page.has_more.then(|| page.next_offset.to_string()),
        },
        talker: q.talker.clone(),
    })))
}
