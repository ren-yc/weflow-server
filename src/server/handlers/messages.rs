//! GET /api/v1/messages — 原生/富数据形状的消息查询。
//!
//! ChatLab 形状走 /chatlab/messages（见 chatlab_messages）。本模块保留**两个面共用**的查询
//! 部分（参数解析、筛选、排序、切片、导出任务收集）：同一批参数在两个面上给出不同的页，是最难
//! 查的一类漂移 —— 两面各抄一份时，改一处忘另一处不会有任何东西变红。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{MediaEnvelope, MediaObject, MessageNative, MessagesNative, Quote};
use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;
use crate::store::Store;

/// 一条导出任务：(local_id, kind, md5, server_id, talker)。
pub(crate) type ExportJob = (i64, crate::parser::MediaKind, Option<String>, i64, String);

/// 一次消息查询的参数。
pub(crate) struct MessageQuery {
    pub talker: String,
    pub limit: usize,
    pub offset: usize,
    pub keyword: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl MessageQuery {
    pub(crate) fn from_params(params: &HashMap<String, String>) -> ApiResult<Self> {
        let talker = params
            .get("talker")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::bad_request("talker is required"))?
            .clone();
        Ok(Self {
            talker,
            limit: crate::server::parse_limit(params, "limit", 100, 10000),
            offset: crate::server::parse_offset(params, "offset"),
            keyword: params
                .get("keyword")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase()),
            start: params.get("start").and_then(|s| crate::server::parse_time_bound(s)),
            // 上界取**当天末刻**：end=20250101 读作「到 1 月 1 日为止」，
            // 取当天 0 点会让那一整天被静默排除在外（回归见 tests/api_smoke.rs 的
            // messages_end_date_covers_the_whole_day）。
            end: params.get("end").and_then(|s| crate::server::parse_time_bound_end(s)),
        })
    }

    /// 消息面用：cursor 优先于 offset（解析不了就退回 offset，与发现面同规）。
    pub(crate) fn from_params_with_cursor(params: &HashMap<String, String>) -> ApiResult<Self> {
        let mut q = Self::from_params(params)?;
        if let Some(c) = params.get("cursor").and_then(|c| c.parse::<usize>().ok()) {
            q.offset = c;
        }
        Ok(q)
    }
}

/// 本页：会话内下标（**降序**，原生面顺序）＋ 翻页信息。
pub(crate) struct MessagePage {
    pub indices: Vec<usize>,
    pub has_more: bool,
    pub next_offset: usize,
}

impl MessagePage {
    pub(crate) fn count(&self) -> usize {
        self.indices.len()
    }
}

fn sort_key(m: &crate::store::MessageRecord) -> (i64, i64, i64) {
    (m.create_time, m.sort_seq, m.local_id)
}

/// 筛选（时间窗 + 关键词）→ 降序 → 偏移切片。
pub(crate) fn page_slice(store: &Store, q: &MessageQuery) -> ApiResult<MessagePage> {
    let conv = store
        .convs
        .get(&q.talker)
        .ok_or_else(|| ApiError::not_found(format!("conversation '{}' not found", q.talker)))?;
    let mut idx: Vec<usize> = conv
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let (t, _s, _l) = sort_key(m);
            (q.start.is_none_or(|s| t >= s)) && (q.end.is_none_or(|e| t <= e))
        })
        .map(|(i, _)| i)
        .collect();
    if let Some(kw) = &q.keyword {
        idx.retain(|&i| {
            let m = &conv[i];
            m.parsed.parsed_text.to_lowercase().contains(kw)
                || m.parsed.raw_content.to_lowercase().contains(kw)
        });
    }
    idx.sort_by(|&a, &b| sort_key(&conv[b]).cmp(&sort_key(&conv[a])));

    let total = idx.len();
    let indices: Vec<usize> = idx.into_iter().skip(q.offset).take(q.limit).collect();
    let next_offset = q.offset + indices.len();
    Ok(MessagePage {
        has_more: next_offset < total,
        indices,
        next_offset,
    })
}

/// media=1 是否要求导出。
pub(crate) fn media_requested(params: &HashMap<String, String>) -> bool {
    crate::server::flex_bool(params, "media")
}

/// media=1 的 kind 白名单。
///
/// 文件附件**永不导出**：本接口承诺的媒体是 image / voice / video / emoji，而 file 的源不是
/// 媒体流（它只是一段被引用过的路径）。这条判断必须排在「没有指定子类型就全都要」之前 ——
/// 否则一个裸 media=1 会把文件附件也放行。
pub(crate) fn wants_kind(params: &HashMap<String, String>, kind: crate::parser::MediaKind) -> bool {
    use crate::parser::MediaKind as K;
    if kind == K::File {
        return false;
    }
    let any_sub = ["image", "voice", "video", "emoji"]
        .iter()
        .any(|k| crate::server::flex_bool(params, k));
    if !any_sub {
        return true;
    }
    match kind {
        K::Image => crate::server::flex_bool(params, "image"),
        K::Voice => crate::server::flex_bool(params, "voice"),
        K::Video => crate::server::flex_bool(params, "video"),
        K::Emoji => crate::server::flex_bool(params, "emoji"),
        K::File => false,
    }
}

/// 收集本页的导出任务。**必须在持有 store 读 guard 的 scope 内调用**。
pub(crate) fn collect_export_jobs(
    store: &Store,
    page: &MessagePage,
    talker: &str,
    params: &HashMap<String, String>,
) -> Vec<ExportJob> {
    let Some(conv) = store.convs.get(talker) else {
        return Vec::new();
    };
    let mut jobs: Vec<ExportJob> = Vec::new();
    for &i in &page.indices {
        let m = &conv[i];
        let Some(hint) = m.parsed.media.as_ref() else {
            continue;
        };
        if !wants_kind(params, hint.kind) {
            continue;
        }
        // 没有 md5 就没法定位源文件（语音例外：它按服务端序号取）。
        if hint.md5.is_none() && hint.kind != crate::parser::MediaKind::Voice {
            continue;
        }
        jobs.push((m.local_id, hint.kind, hint.md5.clone(), m.server_id, talker.to_string()));
    }
    jobs.truncate(200); // bound latency per request
    jobs
}

/// 执行一批导出任务（阻塞池）。返回 localId → 导出结果。
///
/// 调用点必须在**读 guard 的 scope 之外**：guard 不是 Send，跨 await 拿着它编译不过。
pub(crate) async fn run_export_batch(
    state: &Arc<AppState>,
    account: &Arc<crate::server::AccountHandle>,
    jobs: Vec<ExportJob>,
) -> HashMap<i64, crate::media::export::ExportedMedia> {
    if jobs.is_empty() {
        return HashMap::new();
    }
    let account_dir = account.info.dir.clone();
    let export_dir = state.cfg.media_export_dir.clone();
    let mk = account.media_keys;
    let sync = account.sync.clone();
    tokio::task::spawn_blocking(move || {
        sync.lock().export_media_batch(
            &account_dir,
            mk,
            std::path::Path::new(&export_dir),
            &jobs,
            200,
        )
    })
    .await
    .unwrap_or_default()
}

fn message_dto(store: &Store, m: &crate::store::MessageRecord) -> MessageNative {
    // 媒体元数据只要解析得出就带上（WeFlow 形状）；导出的 url / localPath / mediaId 由导出
    // 管线在 media=1 时**填进 struct 字段**（类型化赋值 —— 键名写错就编译不过）。
    let media = m.parsed.media.as_ref().map(|media| MediaObject {
        exported: None,
        file_name: media.file_name.clone(),
        local_path: None,
        md5: media.md5.clone(),
        media_id: None,
        r#type: media.kind.as_str().to_string(),
        url: None,
    });
    // localType 保留平台原始打包值（下游已依赖）。微信 4.x 把
    // (appmsgSubtype << 32) | baseType 打进这一个整数，因此两个半边各自作为只读字段下发，
    // 免得每个消费方都要硬编码 21474836529 这类打包常量才能认出链接卡片。
    let (base_type, appmsg_subtype) = crate::parser::split_local_type(m.local_type);
    MessageNative {
        appmsg_subtype,
        base_type,
        content: m.parsed.display.clone(),
        create_time: m.create_time,
        is_send: if m.is_send { 1 } else { 0 },
        local_id: m.local_id,
        local_type: m.local_type,
        media,
        parsed_content: m.parsed.parsed_text.clone(),
        quote: m.parsed.quote.as_ref().map(|q| Quote {
            account_name: store.session_display(&q.sender),
            content: q.content.clone(),
            platform_message_id: q.platform_message_id.clone(),
            sender: q.sender.clone(),
            r#type: q.msg_type,
        }),
        raw_content: m.parsed.raw_content.clone(),
        reply_to_message_id: m.parsed.reply_to.clone(),
        sender_name: m.sender_name.clone(),
        sender_username: m.sender_username.clone(),
        server_id: m.server_id.to_string(),
        sort_seq: m.sort_seq,
    }
}

#[axum::debug_handler]
pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<axum::response::Response> {
    let params = extract_params(&query, body);
    // 鉴权只看**查询串**：POST body 不是鉴权通道 —— 它连「这是谁的凭据」都区分不出来。
    require_auth(&state, &query, &headers)?;
    let account = ready_account(&state, &params)?;
    let q = MessageQuery::from_params(&params)?;
    let include_media = media_requested(&params);

    // store 的读 guard 不是 Send：所有碰索引的事都在这个 scope 内做完，出 scope 再 await。
    let (page, export_jobs, mut messages) = {
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
        let messages: Vec<MessageNative> =
            page.indices.iter().map(|&i| message_dto(&store, &conv[i])).collect();
        (page, jobs, messages)
    };

    let exported = run_export_batch(&state, &account, export_jobs).await;
    let mut exported_count = 0usize;
    for msg in &mut messages {
        let Some(res) = exported.get(&msg.local_id) else {
            continue;
        };
        if let Some(media) = msg.media.as_mut() {
            // **按名取字节的地址只在两个条件同时成立时给**：本次请求确实写出了本地文件
            // （外链不算），且名字**由内容摘要派生**。按名解析是跨会话的 —— 平台给的名字
            // （语音的 svr_id、视频的 DB 名）在别的会话里可能有同名异内容的文件，那时
            // url 与 mediaId 指向的地址会 404，而「出现即可取」是本服务的承诺。
            //
            // 相对路径还有一个好处：调用方按自己的基址拼接，反代或换端口都不会下发一个失效的
            // 绝对地址；且它**不带 token** —— token 一旦进了响应体就会出现在客户端日志、
            // 中间缓存与任何转发里。
            match &res.external_url {
                Some(u) => media.url = Some(u.clone()),
                None if res.digest_named => {
                    media.url = Some(crate::media::export::exported_media_url(&res.file_name));
                    media.media_id = Some(res.file_name.clone());
                }
                // 非摘要派生的名字：不给任何按名取字节的入口，只给下面那条本地绝对路径。
                None => {}
            }
            media.local_path = (!res.local_path.as_os_str().is_empty())
                .then(|| res.local_path.to_string_lossy().to_string());
            media.exported = Some(true);
            exported_count += 1;
        }
    }

    Ok(axum::response::IntoResponse::into_response(Json(MessagesNative {
        count: page.count(),
        has_more: page.has_more,
        media: MediaEnvelope {
            count: exported_count,
            enabled: include_media,
            export_path: state.cfg.media_export_dir.display().to_string(),
        },
        messages,
        success: true,
        talker: q.talker.clone(),
    })))
}
