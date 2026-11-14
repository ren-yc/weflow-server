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

/// 本页覆盖分片的进度 + 请求的排除线（见 handler 内「排除线」注释）。
fn fresh_progress(
    bounds: std::collections::HashMap<u64, i64>,
    excluded_until: i64,
) -> crate::server::cursor::CursorProgress {
    if excluded_until == i64::MIN {
        crate::server::cursor::CursorProgress::from_bounds(bounds)
    } else {
        crate::server::cursor::CursorProgress::with_exclusion(bounds, excluded_until)
    }
}

// 游标编解码一律走 `crate::server::cursor`：它把「进程 / 账号 / 会话 / 注册代次 /
// 数据源世代」绑进载荷，失效时返回 409 + `reason`，而不是静默退回时间模式。
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
    // 进度游标模式：给出 cursor 时它取代 since/offset 成为权威起点。
    // 游标上下文：本进程 + 本账号 + 本会话 + (注册代次, 数据源世代)。解码失败
    // 分两种——语法坏是 400，身份/代次不符是 409 + `reason: cursor_reset_required`；
    // 两者都不退回时间模式：静默退回等于让客户端以为续上了、实际从头重放。
    let registration = crate::server::current_generation();
    let source_generation = account.generation.load(std::sync::atomic::Ordering::SeqCst);
    let cursor_ctx = crate::server::cursor::CursorContext::for_session(
        &store.my_wxid,
        &id,
        registration,
        source_generation,
    );
    let cursor_progress = match cursor_param.as_deref() {
        Some(raw) => Some(cursor_ctx.decode(raw).map_err(|e| e.into_api_error())?),
        None => None,
    };

    // 进度游标模式（cursor 给出）下分页以行标识（local_id）为准：过滤用
    // 「local_id > 该行所在分片的进度线」的排他下界，排序与页推进都按 local_id
    // 单调——入库序与时间序在乱序行上分叉时，续拉严格无重放、无跳行；乱序行
    // 的 messages 顺序不再是时间升序，这是「排空判据=入库进度」的直接后果。
    //
    // 进度线按**分片**（source_shard）记账而不是整个会话：local_id 只在分片内
    // 唯一（`MessageRecord::source_shard` 的注释钉着这一事实），同一会话的行
    // 可以来自多个分片表。只编码会话最大值时，低分片的新追加行永远落在
    // 「local_id > 会话最大值」之外且拿不到任何重置信号——静默丢行。
    //
    // **初始 since 窗口的排除随游标携带**（`CursorProgress` 的排除线）：首次
    // 响应只覆盖部分分片时（其余分片在窗口外没有行），游标线上没有它们的进度；
    // 排除线把「初始窗口对未覆盖分片的排除」钉进游标，续拉对未覆盖分片仍按它
    // 过滤——带不带 since 都不会重放窗口外历史（分片 A 时间 30 响应了 since=20，
    // 分片 B 时间 10 的历史行在续拉时不得冒出来）。分片一旦被覆盖，进度线接管。
    // 无 cursor 的时间模式保持规范原行为：(create_time, sort_seq, local_id)
    // 复合键升序、页不跨秒推进（nextSince 语义依赖它），契约不要求与游标同键。
    let cursor_mode = cursor_progress.is_some();
    let mut ascending: Vec<&crate::store::MessageRecord> = conv
        .iter()
        .filter(|m| {
            if let Some(progress) = &cursor_progress {
                let shard = crate::server::cursor::shard_code(&m.source_shard);
                let time_bound = progress.time_bound_for(shard);
                return progress.admits(shard, m.local_id)
                    && time_bound.is_none_or(|s| m.create_time > s)
                    && end.is_none_or(|e| m.create_time <= e);
            }
            since.is_none_or(|s| m.create_time > s) && end.is_none_or(|e| m.create_time <= e)
        })
        .collect();
    if cursor_mode {
        ascending.sort_by_key(|m| m.local_id);
    } else {
        ascending.sort_by(|a, b| {
            (a.create_time, a.sort_seq, a.local_id).cmp(&(b.create_time, b.sort_seq, b.local_id))
        });
    }

    let total = ascending.len();
    // Page from `offset`, extending to the end of the last second's ts group:
    // pages never split a second, which is what lets `nextSince` (the page's
    // last timestamp) advance without dropping the rest of that second.
    let start = if cursor_progress.is_some() { 0 } else { offset.min(total) };
    let mut page_end = start;
    if cursor_progress.is_some() {
        // 游标模式：页推进按 local_id 组——与排序键一致，页边界不切断单调序，
        // 页末 localId 是严格的「已交付上界」。
        let mut prev_lid = None;
        while page_end < total {
            let lid = ascending[page_end].local_id;
            if prev_lid.is_some_and(|p| p != lid) && page_end - start >= limit {
                break;
            }
            prev_lid = Some(lid);
            page_end += 1;
        }
    } else {
        // 时间模式：页不跨秒推进——`nextSince`（页末时间戳）要能无损续页。
        let mut prev_ts = None;
        while page_end < total {
            let ts = ascending[page_end].create_time;
            if prev_ts.is_some_and(|p| p != ts) && page_end - start >= limit {
                break;
            }
            prev_ts = Some(ts);
            page_end += 1;
        }
    }
    let page: Vec<&crate::store::MessageRecord> = ascending[start..page_end].to_vec();
    let has_more = page_end < total;
    // The page's OWN last timestamp. Using the whole set's maximum here would
    // tell the client to resume past everything it has not seen yet.
    let next_since = page.last().map(|m| m.create_time).unwrap_or(since.unwrap_or(0));
    // 进度游标编码「**已交付集合**」的进度上界——不是页末行：
    //
    // - cursor 模式：排序按 local_id 单调，页末行就是已交付集合的最大 local_id，
    //   两者重合；进度线**按分片记账**（local_id 只在分片内唯一），本页覆盖的
    //   分片取「已交付集合的最大值」，没覆盖的分片保留请求游标里的旧值——
    //   进度对每个分片都只前进不回退。空页时整条进度线保持请求原值。
    // - 时间模式非空页：排序按时间复合键，页末行 local_id **可能不是已交付集合
    //   里最大的**（回溯入库的行：时间早、行标识大）。编码页末行会让后续游标链
    //   重放它；必须编码「本次响应已交付的行里最大的 local_id」。
    // - 时间模式空页：本响应没有交付任何行，但请求窗口 (since, end] 为空本身
    //   就是进度陈述——窗口内无行，则一切 create_time ≤ end 的行都已确立。
    //   游标编码「end 界内已确立的最大 local_id」（end 缺省时水位线=now 覆盖
    //   全部行）。编码 0 是「从头拉取」，周期轮询的客户端采纳它再丢掉时间过滤
    //   就会重放全历史。
    let request_progress = cursor_progress.clone().unwrap_or_default();
    // 本请求的排除线：cursor 模式沿用请求游标里的线（merged 只收紧），时间模式
    // 取本请求的 since（无 since = 无排除 = i64::MIN）。fresh 的排除线只在
    // 「请求带 since」时进入下一游标——不带 since 的纯增量续拉不得凭空引入排除。
    let fresh_exclusion = if cursor_progress.is_some() {
        // 游标模式：排除线已在 request_progress 里，fresh 不再携带。
        i64::MIN
    } else {
        since.unwrap_or(i64::MIN)
    };
    let next_progress = if cursor_progress.is_some() {
        if page.is_empty() {
            // 空页：进度保持（游标值不变，不回退为从头拉取）。
            request_progress
        } else {
            // 本页覆盖的分片推进到已交付集合的最大值，其余分片保留旧值。
            let mut bounds = std::collections::HashMap::<u64, i64>::new();
            for m in &page {
                let shard = crate::server::cursor::shard_code(&m.source_shard);
                let slot = bounds.entry(shard).or_insert(m.local_id);
                *slot = (*slot).max(m.local_id);
            }
            request_progress.merged(&fresh_progress(bounds, fresh_exclusion))
        }
    } else if page.is_empty() {
        let mut bounds = std::collections::HashMap::<u64, i64>::new();
        for m in conv.iter().filter(|m| m.create_time <= watermark) {
            let shard = crate::server::cursor::shard_code(&m.source_shard);
            let slot = bounds.entry(shard).or_insert(m.local_id);
            *slot = (*slot).max(m.local_id);
        }
        fresh_progress(bounds, fresh_exclusion)
    } else {
        let mut bounds = std::collections::HashMap::<u64, i64>::new();
        for m in &page {
            let shard = crate::server::cursor::shard_code(&m.source_shard);
            let slot = bounds.entry(shard).or_insert(m.local_id);
            *slot = (*slot).max(m.local_id);
        }
        fresh_progress(bounds, fresh_exclusion)
    };
    let next_cursor = cursor_ctx.encode(&next_progress);

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
