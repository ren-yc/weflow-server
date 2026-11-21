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
    // `since=0` 与省略等价：0 是纪元起点，挡不住任何真实行，却会让游标与回显带上
    // 一个假下界（`sync.since: 0` 读起来像「从 1970 年起」）。在建链入口归一化成
    // 「无下界」，游标里因此统一存 `i64::MIN` 哨兵。回归：`pull_since_zero_is_no_window`。
    let since = since.filter(|s| *s != 0);
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

    // 两个代次与会话内容必须在**同一把锁**下取：store 的读锁就是生命周期闸门——注销要
    // 拿写锁，并按「清空 store → 推进注册代次」的顺序走完。持读锁期间两件事都发生不了，
    // 锁内采样得到的就是同一时刻的一致视图；锁外采样会留下「新代次 + 旧快照」或反过来的
    // 混合身份，签出的游标要么 fail-open（下一请求该 409 却能续），要么中途 409 打断拉取。
    let store = account.store.read();
    let source_generation = account.generation.load(std::sync::atomic::Ordering::SeqCst);
    let registration = crate::server::current_generation();
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

    // ── 两段式游标：第一段窗口快照扫描，第二段入库补发（契约 0.12.0）──────
    //
    // 首页按时间键交付、续页只按行标识过滤时，两种序下的「已交付集合」互不为前缀：
    // 时间更晚、行标识更小的行会被 `行标识 > 水位` 跨过去、此后永不交付。两段式把
    // 集合按「链开始时的行标识上界」切干净，每段内部只剩一种序——第一段时间键、
    // 第二段行标识——「已交付 = 该序的前缀」才成立。判据与载荷见 `server::cursor`。
    //
    // 进度线按**分片**（source_shard）记账：local_id 只在分片内唯一
    // （`MessageRecord::source_shard` 的注释钉着这一事实），同一会话的行可以来自
    // 多个分片表。快照取**已索引**各分片的最大行标识——索引事后补齐的行行标识
    // 必然更大，因此落第二段；取库文件上界会把它们冻在第一段里又被键过滤掉。
    //
    // 回归：`cursor_continuation_is_monotonic_when_row_order_diverges_from_time_order`
    // （乱序行经链可达、不重放）、`cursor_end_window_does_not_freeze_the_chain`
    // （`end` 挡住的行留在第一段）、契约用例 `cursor-drain-equals-full-out-of-order`。
    let mut bounds: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
    for m in conv.iter() {
        let shard = crate::server::cursor::shard_code(&m.source_shard);
        let slot = bounds.entry(shard).or_insert(m.local_id);
        *slot = (*slot).max(m.local_id);
    }
    let snapshot: Vec<(u64, i64)> = bounds.into_iter().collect();

    // 窗口下界随游标走（它表达「这条链从哪儿开始」）；`end` 每页活跃、不进游标。
    let fresh_chain = cursor_progress.is_none();
    let excluded_since = cursor_progress
        .as_ref()
        .map_or(since.unwrap_or(i64::MIN), |p| p.excluded_since());
    let progress = cursor_progress
        .unwrap_or_else(|| crate::server::cursor::CursorProgress::start(snapshot.clone(), since));

    // 第一段候选：窗口内 + 快照内 + 键在位置之后，按时间键升序。
    // 第一段**不推进**快照——推进会把「已被页切掉、但键更小」的新行跨过去。
    let phase1 = |bound_end: Option<i64>| -> Vec<&crate::store::MessageRecord> {
        let mut rows: Vec<&crate::store::MessageRecord> = conv
            .iter()
            .filter(|m| {
                m.create_time > excluded_since
                    && bound_end.is_none_or(|e| m.create_time <= e)
                    && progress.admits1(
                        crate::server::cursor::shard_code(&m.source_shard),
                        crate::server::cursor::TimeKey {
                            create_time: m.create_time,
                            sort_seq: m.sort_seq,
                            local_id: m.local_id,
                        },
                    )
            })
            .collect();
        rows.sort_by_key(|m| (m.create_time, m.sort_seq, m.local_id));
        rows
    };
    // 第二段候选：行标识在快照之后，**不看时间**——这一段就是「链开始后入库的行
    // 无论时间戳都能经游标拿到」的载体。按 (分片, 行标识) 升序，页因此是「各分片
    // 行标识前缀」的并，水位推进不会跨过未交付的行。
    let phase2 = || -> Vec<&crate::store::MessageRecord> {
        let mut rows: Vec<&crate::store::MessageRecord> = conv
            .iter()
            .filter(|m| {
                progress.admits2(
                    crate::server::cursor::shard_code(&m.source_shard),
                    m.local_id,
                )
            })
            .collect();
        rows.sort_by_key(|m| (crate::server::cursor::shard_code(&m.source_shard), m.local_id));
        rows
    };
    // 页边界：第一段**不切秒组**（`nextSince` 要能无损续页，等键行也必须同页）；
    // 第二段按行标识逐个推进，没有秒组可切。
    let page_end_for = |rows: &[&crate::store::MessageRecord], split_second: bool| -> usize {
        if !split_second {
            return rows.len().min(limit);
        }
        let mut end = 0usize;
        let mut prev_ts: Option<i64> = None;
        while end < rows.len() {
            let ts = rows[end].create_time;
            if prev_ts.is_some_and(|p| p != ts) && end >= limit {
                break;
            }
            prev_ts = Some(ts);
            end += 1;
        }
        end
    };
    let delivered_bounds = |page: &[&crate::store::MessageRecord]| -> Vec<(u64, i64)> {
        page.iter()
            .map(|m| (crate::server::cursor::shard_code(&m.source_shard), m.local_id))
            .collect()
    };

    let mut page: Vec<&crate::store::MessageRecord> = Vec::new();
    let next_progress;
    let mut has_more = false;
    // 第一段（`end` 视作无上界）还剩多少条：空页分支与末尾的 `hasMore` 判据都要它，
    // 算一次就够。这两处只问「还有没有」与「是不是都已消费完」，不需要行本身——
    // 收集 + 排序在这里是白做（空续页路径每次请求要扫两遍）。
    let phase1_pending = || -> usize {
        conv.iter()
            .filter(|m| {
                m.create_time > excluded_since
                    && progress.admits1(
                        crate::server::cursor::shard_code(&m.source_shard),
                        crate::server::cursor::TimeKey {
                            create_time: m.create_time,
                            sort_seq: m.sort_seq,
                            local_id: m.local_id,
                        },
                    )
            })
            .count()
    };
    // `offset` 只在**无游标**的翻页里生效：游标链的进度由游标承载，混用两者会让
    // 「已交付」出现两个互不同步的来源。
    let pending_phase1 = phase1_pending();
    let mut start = 0usize;
    if progress.phase() == 2 {
        let rows = phase2();
        let end = page_end_for(&rows, false);
        page = rows[..end].to_vec();
        has_more = end < rows.len();
        next_progress = progress.advance_phase2(delivered_bounds(&page));
    } else {
        let rows = phase1(end);
        let skip = if fresh_chain { offset.min(rows.len()) } else { 0 };
        let rows = &rows[skip..];
        if !rows.is_empty() {
            let end = page_end_for(rows, true);
            page = rows[..end].to_vec();
            has_more = end < rows.len();
            start = skip;
            // 页末行的时间键就是本页的最大键：页不切秒组，排序键与页推进一致。
            let last = page.last().expect("非空页必有末行");
            next_progress = progress.with_position(crate::server::cursor::TimeKey {
                create_time: last.create_time,
                sort_seq: last.sort_seq,
                local_id: last.local_id,
            });
        } else if fresh_chain {
            // 无游标的空页 = **空页结算**：`end` 界内的一切都已确立。位置时间取
            // `watermark`（`end` 缺省时是当前时刻），时间更大的行键更大、仍然可达
            // ——「被 `end` 挡住的行没有丢」正落在这里。
            let max_lid = conv
                .iter()
                .filter(|m| m.create_time <= watermark)
                .map(|m| m.local_id)
                .max()
                .unwrap_or(0);
            next_progress = crate::server::cursor::CursorProgress::settled(
                watermark,
                max_lid,
                snapshot.clone(),
                since,
            );
        } else if pending_phase1 > 0 {
            // 被 `end` 挡住不算耗尽：那些行键大于位置，客户端抬高 `end` 后照样交付。
            // 在这里切段会把它们永久留在第一段之外（第二段只认行标识大于快照）。
            next_progress = progress.clone();
        } else {
            // 第一段确实耗尽 ⇒ 切第二段，本页直接交付第二段的行。不返回空页：
            // 「排空即停」的客户端会把空页当成链结束，第二段的行就永远拿不到。
            let rows = phase2();
            let end = page_end_for(&rows, false);
            page = rows[..end].to_vec();
            has_more = end < rows.len();
            next_progress = progress.into_phase2().advance_phase2(delivered_bounds(&page));
        }
    }
    // 第一段耗尽、但第二段已有行 ⇒ 仍要报「还有更多」：否则「排空即停」的客户端
    // 永远看不到第二段（链开始后入库的行），而它正是游标承诺的一部分。判据是
    // 「本请求已把第一段（`end` 视作无上界）消费完」——只看 `phase1(None)` 是否为空
    // 会把「本页正好交付完窗口内的行」误判成没耗尽：那时它恰好等于本页那几行。
    // 被 `end` 挡住不算耗尽：那些行属于第一段，客户端抬高 `end` 才该继续。
    if !has_more && next_progress.phase() == 1 && pending_phase1 <= start + page.len() {
        has_more = !phase2().is_empty();
    }
    // 第二段按行标识排序，页末行的时间会回退——按它续拉会把已交付的行重放一遍。
    // 因此第二段一律给出 `watermark`；第一段沿用「页末时间戳 / 排空取 watermark」。
    let next_since = if next_progress.phase() == 2 || !has_more {
        watermark
    } else {
        page.last().map(|m| m.create_time).unwrap_or(watermark)
    };
    // `offset` 只在无游标、且时间戳无法推进的退化页里承载进度；游标模式恒为 0。
    let next_offset = if fresh_chain && has_more && next_since <= since.unwrap_or(i64::MIN) {
        start.saturating_add(page.len())
    } else {
        0
    };
    let next_cursor = cursor_ctx.encode(&next_progress).map_err(|e| e.into_api_error())?;

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
            next_since,
            // `i64::MIN` 是「无下界」的内部哨兵，0 是归一化之前签出的旧游标里才可能
            // 出现的值；两者都回显 `null`，其余原样回显。哨兵不外泄（契约 0.13.0）。
            since: match excluded_since {
                i64::MIN | 0 => None,
                bound => Some(bound),
            },
            // Both cursors are meant to be echoed back verbatim, so they must
            // not skip the same rows twice. `nextSince` is exclusive and the
            // page ends on a complete ts group, so re-filtering with it drops
            // exactly the rows already served — leaving the next unseen row at
            // offset 0. `nextOffset` therefore only carries weight in the
            // degenerate case where the timestamp could not advance at all,
            // and never on a cursor chain（进度由游标承载）。
            next_offset,
            watermark,
        },
    };
    Ok(Json(body))
}
