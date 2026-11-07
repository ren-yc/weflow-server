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

/// 编码不透明进度游标：`v1.<generation>.<localId>` 的 base64。内容对客户端
/// 不透明；排他性由 localId 保证（比它小的行已全部交付），账号代次保证跨代次
/// 的旧游标被识别为「从头拉取」而不是「错误续页」。
fn encode_cursor(generation: u64, local_id: i64) -> String {
    use std::io::Write;
    let mut buf = Vec::new();
    let _ = write!(buf, "v1.{}.{}", generation, local_id);
    base64_simply(&buf)
}

/// 解码本端点编出的游标；无法识别返回 None，调用方退回时间模式——不是错误，
/// 兼容期客户端可能仍用 nextSince。代次不匹配按「从头拉取」处理（localId 0）。
fn decode_cursor(cursor: &str, generation: u64) -> Option<i64> {
    let raw = decode_base64_simply(cursor)?;
    let text = String::from_utf8(raw).ok()?;
    let rest = text.strip_prefix("v1.")?;
    let mut parts = rest.splitn(2, '.');
    let token_gen: u64 = parts.next()?.parse().ok()?;
    let local_id: i64 = parts.next()?.parse().ok()?;
    if token_gen != generation {
        return Some(0);
    }
    Some(local_id)
}

fn base64_simply(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn decode_base64_simply(text: &str) -> Option<Vec<u8>> {
    let rev = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes: Vec<u8> = text.bytes().filter(|b| *b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            n |= rev(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

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
    // 进度游标（契约 0.9.0 的 opaqueCursor 能力）：给出时取代 since/offset。
    let cursor_param = query.get("cursor").cloned();

    let export_dir = state.cfg.media_export_dir.clone();
    let session_id = id.clone();
    let digest_handles = tokio::task::spawn_blocking(move || {
        crate::media::export::digest_handles(&export_dir, &session_id)
    })
    .await
    .unwrap_or_default();

    let store = account.store.read();
    // 存在性语义与消息面同款（0.10.0 起）：索引不认识这个 id 才 404；
    // 「名册里有、零条消息」返回空页（sync.hasMore=false），与 404 分离。
    if !store.conversation_known(&id) {
        return Err(ApiError::not_found(format!("session '{id}' not found")));
    }
    let empty = Vec::new();
    let conv = store.convs.get(&id).unwrap_or(&empty);

    // Upper bound of this pull, echoed as `watermark`: a client that has
    // drained the session resumes from here. It is a TIME BOUND, not the
    // newest message's timestamp, so an idle session still reports progress.
    let watermark = end.unwrap_or_else(|| chrono::Utc::now().timestamp());
    // 进度游标模式：给出 cursor 时它取代 since/offset 成为权威起点。代次与
    // 本端不匹配时按「从头拉取」处理（旧账号的进度线在新账号上不存在）。
    let generation = crate::server::current_generation();
    let cursor_local_id = cursor_param.as_deref().and_then(|c| decode_cursor(c, generation));

    // Chronological messages within (since, end] — `since` is EXCLUSIVE, so a
    // client resuming with `nextSince` never re-fetches the boundary second
    // and can neither loop nor see duplicates (qqflow-server parity).
    // 进度游标是权威起点：给出时忽略 since/offset（时间只是元数据，入库进度
    // 才是增量判据）。排空后同秒晚入库的行由 localId 排他性覆盖。
    let mut ascending: Vec<&crate::store::MessageRecord> = conv
        .iter()
        .filter(|m| {
            if let Some(lid) = cursor_local_id {
                return m.local_id > lid && end.is_none_or(|e| m.create_time <= e);
            }
            since.is_none_or(|s| m.create_time > s) && end.is_none_or(|e| m.create_time <= e)
        })
        .collect();
    ascending.sort_by(|a, b| {
        (a.create_time, a.sort_seq, a.local_id).cmp(&(b.create_time, b.sort_seq, b.local_id))
    });

    let total = ascending.len();
    // Page from `offset`, extending to the end of the last second's ts group:
    // pages never split a second, which is what lets `nextSince` (the page's
    // last timestamp) advance without dropping the rest of that second.
    let start = if cursor_local_id.is_some() { 0 } else { offset.min(total) };
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
    // 进度游标恒编码**页末行**（排空时=最后交付行）的 localId：下一次增量的
    // 排他起点，排空后仍在场。排空后同秒晚入库的行 localId 更大，续拉必然
    // 可达——这正是时间游标补不上的盲区。
    let last_local_id = page.last().map(|m| m.local_id).unwrap_or(cursor_local_id.unwrap_or(0));
    let next_cursor = encode_cursor(generation, last_local_id);

    // `groupNickname` is the sender's per-chatroom card, which lives in
    // `group_cards` — NOT the contact's 备注. The two are different things and
    // serving the remark here made `groupNickname` wrong for every group
    // member who had a remark but no card (and for every private chat).
    let chatroom = id.ends_with("@chatroom").then_some(id.as_str());

    // members = senders in this page (dedup)。字段取法与消息面共用一处（server::chatlab）：
    // 两处各写一遍时，「取联系人档案还是取消息里的昵称」这种分歧不会有任何东西变红。
    // 本会话导出目录里的摘要名集合，**一次请求算一次**（逐条 stat 会让一页变成
    // 几十次目录打开）。目录扫描是阻塞 IO ⇒ 交给 spawn_blocking，与按名取字节那条路由同规。

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
            // 「出现即可取」的判据与 SSE 那一路同源：只有**确实落盘**、且名字**由内容摘要
            // 派生**的那些才配当句柄。两处各写一遍时，「一面悄悄放宽」不会有任何东西变红，
            // 而放宽的那一面会通告必 404 的 id —— 调用方拿到 404 只会以为服务坏了。
            //
            // 按**摘要干**查（`digest_handles`），不是拿元数据名去 stat：图片的落盘名扩展名是
            // 解码后嗅探出来的（可能与 XML 属性名不同，wxgf 还可能被转成 png），按全名 stat 会
            // 漏报 —— 而漏报是静默的：调用方只会多跑一趟导出。语音的 `voice_<svr>.silk` 与视频
            // 的平台名不是摘要派生 ⇒ 天然不在表里 ⇒ 省略（与它们不能作句柄的裁决一致）。
            let media_id = m.parsed.media.as_ref().and_then(|hint| {
                let stem = hint.file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&hint.file_name);
                if stem.len() != 32 {
                    return None;
                }
                digest_handles.get(&stem.to_ascii_lowercase()).cloned()
            });
            PullMessage {
                account_name: f.account_name,
                content: f.content,
                group_nickname: f.group_nickname,
                media: f.media,
                media_id,
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
            next_cursor: Some(next_cursor),
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
