//! GET/POST /api/v1/messages — query a conversation with filters, optional
//! ChatLab output. WeFlow-compatible field shapes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::server::dto::{
    ChatlabHeader, ChatlabMember, ChatlabMessage, ChatlabMeta, MediaEnvelope, MediaObject,
    MessageNative, MessagesChatlab, MessagesNative, Quote,
};
use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;
use crate::store::Store;

fn sort_key(m: &crate::store::MessageRecord) -> (i64, i64, i64) {
    (m.create_time, m.sort_seq, m.local_id)
}

fn message_dto(store: &Store, m: &crate::store::MessageRecord) -> MessageNative {
    // 媒体元数据只要解析得出就带上（WeFlow 形状）；导出的 url / localPath 由导出
    // 管线在 `media=1` 时**填进 struct 字段**（类型化赋值 —— 键名写错就编译不过，
    // 而早先按字符串键 `Value::insert` 时写错只会静默多/少一个键）。
    let media = m.parsed.media.as_ref().map(|media| MediaObject {
        exported: None,
        file_name: media.file_name.clone(),
        local_path: String::new(),
        md5: media.md5.clone(),
        r#type: media.kind.as_str().to_string(),
        url: String::new(),
    });
    // `localType` stays the raw packed value downstream already pins. WeChat 4.x
    // packs `(appmsgSubtype << 32) | baseType` into it, so the two halves are
    // published as separate read-only fields rather than making every consumer
    // hardcode packed constants like 21474836529 to recognise a link card.
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
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<axum::response::Response> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;

    let talker = params
        .get("talker")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("talker is required"))?
        .clone();
    let limit = crate::server::parse_limit(&params, "limit", 100, 10000);
    let offset = crate::server::parse_offset(&params, "offset");
    let keyword = params.get("keyword").filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
    let chatlab = crate::server::flex_bool(&params, "chatlab")
        || params.get("format").map(|f| f.eq_ignore_ascii_case("chatlab")).unwrap_or(false);
    let include_media = crate::server::flex_bool(&params, "media")
        || crate::server::flex_bool(&params, "meiti")
        || chatlab;

    let start = params.get("start").and_then(|s| crate::server::parse_time_bound(s));
    // 上界取**当天末刻**：`end=20250101` 读作「到 1 月 1 日为止」，
    // 取当天 0 点会让那一整天被静默排除在外（回归见 tests/api_smoke.rs 的
    // messages_end_date_covers_the_whole_day）。
    let end = params.get("end").and_then(|s| crate::server::parse_time_bound_end(s));

    // Everything that touches the store happens inside this scoped block so
    // the (non-Send) read guard can never be held across the await below.
    let (count, has_more, export_jobs, mut messages) = {
        let store = account.store.read();
        let conv = store
            .convs
            .get(&talker)
            .ok_or_else(|| ApiError::not_found(format!("conversation '{talker}' not found")))?;

        // filter + sort (descending, newest first)
        let mut idx: Vec<usize> = conv.iter().enumerate().filter(|(_, m)| {
            let (t, _s, _l) = sort_key(m);
            (start.is_none_or(|s| t >= s)) && (end.is_none_or(|e| t <= e))
        }).map(|(i, _)| i).collect();
        if let Some(kw) = &keyword {
            idx.retain(|&i| {
                let m = &conv[i];
                m.parsed.parsed_text.to_lowercase().contains(kw)
                    || m.parsed.raw_content.to_lowercase().contains(kw)
            });
        }
        idx.sort_by(|&a, &b| sort_key(&conv[b]).cmp(&sort_key(&conv[a])));

        let slice = idx.iter().skip(offset).take(limit).map(|&i| &conv[i]).collect::<Vec<_>>();
        let count = slice.len();
        let has_more = offset + slice.len() < idx.len();

        if chatlab {
            // chatlab: ascending order like WeFlow's pull format
            let mut asc = slice.to_vec();
            asc.reverse();
            let (group_id, owner_id) = (talker.clone(), store.my_wxid.clone());
            // `groupNickname` is the per-chatroom card from `group_cards`, not
            // the contact's 备注 — see the same note in `chatlab_pull`.
            let chatroom = talker.ends_with("@chatroom").then_some(talker.as_str());
            let members: Vec<ChatlabMember> = {
                let sender_ids: Vec<&str> = asc.iter().map(|m| m.sender_username.as_str()).collect();
                let mut seen = std::collections::HashSet::new();
                sender_ids
                    .into_iter()
                    .filter(|s| !s.is_empty() && seen.insert(*s))
                    .map(|s| {
                        let c = store.contacts.get(s);
                        ChatlabMember {
                            account_name: c
                                .map(|c| c.display_name())
                                .unwrap_or_else(|| s.to_string()),
                            avatar: c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
                            group_nickname: store.group_card(chatroom, s),
                            platform_id: s.to_string(),
                        }
                    })
                    .collect()
            };
            let msgs: Vec<ChatlabMessage> = asc
                .iter()
                .map(|m| {
                    ChatlabMessage {
                        account_name: m.sender_name.clone(),
                        content: m.parsed.display.clone(),
                        group_nickname: store.group_card(chatroom, &m.sender_username),
                        platform_message_id: m.server_id.to_string(),
                        reply_to_message_id: m.parsed.reply_to.clone(),
                        sender: m.sender_username.clone(),
                        timestamp: m.create_time,
                        r#type: crate::server::handlers::chatlab_type(m.local_type, &m.parsed),
                        // `mediaPath` 有意不输出：安装版契约里有这个键，但本项目
                        // 无法给出有意义的值（媒体导出由 `media=1` 开关控制，且
                        // 只在原生形状回填），恒空的键比没有键更容易误导。
                        // 媒体字节走本接口的 `media` 对象 + /api/v1/media/{id}。
                    }
                })
                .collect();
            let body = MessagesChatlab {
                chatlab: ChatlabHeader {
                    exported_at: chrono::Utc::now().timestamp(),
                    generator: "weflow-server".to_string(),
                    version: "0.0.2".to_string(),
                },
                count: slice.len(),
                has_more,
                members,
                messages: msgs,
                meta: ChatlabMeta {
                    group_id,
                    name: store.session_display(&talker),
                    owner_id,
                    platform: "wechat".to_string(),
                    r#type: if talker.ends_with("@chatroom") { "group" } else { "private" }
                        .to_string(),
                },
                success: true,
                talker: talker.clone(),
            };
            return Ok(axum::response::IntoResponse::into_response(Json(body)));
        }

        // ---- media export job collection ----
        let mut export_jobs: Vec<(i64, crate::parser::MediaKind, Option<String>, i64, String)> =
            Vec::new();
        if include_media {
            let any_sub = ["image", "tupian", "voice", "vioce", "video", "emoji"]
                .iter()
                .any(|k| crate::server::flex_bool(&params, k));
            let want = |kind: crate::parser::MediaKind| -> bool {
                use crate::parser::MediaKind as K;
                // Files never export: the documented contract covers image /
                // voice / video / emoji only. This test must come *before* the
                // `!any_sub` shortcut — a bare `media=1` leaves `any_sub` false
                // and would otherwise wave every kind through, never reaching
                // the `K::File => false` arm below. Until now that path was
                // closed only by accident, because file hints carried no md5 and
                // tripped the md5 gate; now that the md5 is populated, this is
                // the only thing keeping 3530 file payloads off disk.
                if kind == K::File {
                    return false;
                }
                if !any_sub {
                    return true;
                }
                match kind {
                    K::Image => {
                        crate::server::flex_bool(&params, "image")
                            || crate::server::flex_bool(&params, "tupian")
                    }
                    K::Voice => {
                        crate::server::flex_bool(&params, "voice")
                            || crate::server::flex_bool(&params, "vioce")
                    }
                    K::Video => crate::server::flex_bool(&params, "video"),
                    K::Emoji => crate::server::flex_bool(&params, "emoji"),
                    K::File => false,
                }
            };
            for m in &slice {
                let Some(hint) = m.parsed.media.as_ref() else {
                    continue;
                };
                if !want(hint.kind) {
                    continue;
                }
                if hint.md5.is_none() && hint.kind != crate::parser::MediaKind::Voice {
                    continue;
                }
                export_jobs.push((m.local_id, hint.kind, hint.md5.clone(), m.server_id, talker.clone()));
            }
            export_jobs.truncate(200); // bound latency per request
        }

        let messages: Vec<MessageNative> =
            slice.iter().map(|m| message_dto(&store, m)).collect();
        (count, has_more, export_jobs, messages)
    };

    let exported: std::collections::HashMap<i64, crate::media::export::ExportedMedia> =
        if export_jobs.is_empty() {
            Default::default()
        } else {
            let account_dir = account.info.dir.clone();
            let export_dir = state.cfg.media_export_dir.clone();
            let mk = account.media_keys;
            let sync = account.sync.clone();
            tokio::task::spawn_blocking(move || {
                sync.lock().export_media_batch(
                    &account_dir,
                    mk,
                    std::path::Path::new(&export_dir),
                    &export_jobs,
                    200,
                )
            })
            .await
            .unwrap_or_default()
        };
    let mut exported_count = 0usize;
    for msg in &mut messages {
        let Some(res) = exported.get(&msg.local_id) else {
            continue;
        };
        // **类型化赋值**：字段名写错编译不过。此前是往 `Value` 里按字符串键 insert，
        // 键名写错不会报错，只会静默改变响应。
        if let Some(media) = msg.media.as_mut() {
            // **相对路径，且不带 token**：token 一旦进了响应体，就会出现在客户端日志、
            // 中间缓存与任何转发里，而它本来是只走请求头的凭据。相对路径还有一个好处——
            // 调用方按自己的基址拼接，反代或换端口都不会下发一个失效的绝对地址。
            let url = match &res.external_url {
                Some(u) => u.clone(),
                None => crate::media::export::exported_media_url(&talker, res.kind_dir, &res.file_name),
            };
            media.url = url;
            media.local_path = res.local_path.to_string_lossy().to_string();
            media.exported = Some(true);
            exported_count += 1;
        }
    }

    Ok(axum::response::IntoResponse::into_response(Json(MessagesNative {
        count,
        has_more,
        media: MediaEnvelope {
            count: exported_count,
            enabled: include_media,
            export_path: state.cfg.media_export_dir.display().to_string(),
        },
        messages,
        success: true,
        talker,
    })))
}

