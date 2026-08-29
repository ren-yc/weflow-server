//! GET/POST /api/v1/messages — query a conversation with filters, optional
//! ChatLab output. WeFlow-compatible field shapes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;
use crate::store::Store;

fn sort_key(m: &crate::store::MessageRecord) -> (i64, i64, i64) {
    (m.create_time, m.sort_seq, m.local_id)
}

fn message_json(store: &Store, _conv: &str, m: &crate::store::MessageRecord, _include_media: bool) -> serde_json::Value {
    // media metadata is always present when parseable (WeFlow shape); export
    // urls/paths are filled in by the export pipeline when media=1
    let media = m.parsed.media.as_ref().map(|media| {
        json!({
            "type": media.kind.as_str(),
            "fileName": media.file_name,
            "md5": media.md5,
            "url": "",
            "localPath": "",
        })
    });
    json!({
        "localId": m.local_id,
        "serverId": m.server_id.to_string(),
        "localType": m.local_type,
        "createTime": m.create_time,
        "sortSeq": m.sort_seq,
        "isSend": if m.is_send { 1 } else { 0 },
        "senderUsername": m.sender_username,
        "senderName": m.sender_name,
        "content": m.parsed.display,
        "rawContent": m.parsed.raw_content,
        "parsedContent": m.parsed.parsed_text,
        "replyToMessageId": m.parsed.reply_to,
        "quote": m.parsed.quote.as_ref().map(|q| json!({
            "platformMessageId": q.platform_message_id,
            "sender": q.sender,
            "accountName": store.session_display(&q.sender),
            "content": q.content,
            "type": q.msg_type,
        })),
        "media": media,
    })
}

#[axum::debug_handler]
pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
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
    let end = params.get("end").and_then(|s| crate::server::parse_time_bound(s));

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
            let members: Vec<serde_json::Value> = {
                let sender_ids: Vec<&str> = asc.iter().map(|m| m.sender_username.as_str()).collect();
                let mut seen = std::collections::HashSet::new();
                sender_ids
                    .into_iter()
                    .filter(|s| !s.is_empty() && seen.insert(*s))
                    .map(|s| {
                        let c = store.contacts.get(s);
                        json!({
                            "platformId": s,
                            "accountName": c.map(|c| c.display_name()).unwrap_or_else(|| s.to_string()),
                            "groupNickname": store.group_card(chatroom, s),
                            "avatar": c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
                        })
                    })
                    .collect()
            };
            let msgs: Vec<serde_json::Value> = asc
                .iter()
                .map(|m| {
                    json!({
                        "sender": m.sender_username,
                        "accountName": m.sender_name,
                        "groupNickname": store.group_card(chatroom, &m.sender_username),
                        "timestamp": m.create_time,
                        "type": crate::server::handlers::chatlab_type(m.local_type, &m.parsed),
                        "content": m.parsed.display,
                        "platformMessageId": m.server_id.to_string(),
                        "replyToMessageId": m.parsed.reply_to,
                        "mediaPath": "",
                    })
                })
                .collect();
            return Ok(Json(json!({
                "success": true,
                "talker": talker,
                "count": slice.len(),
                "hasMore": has_more,
                "chatlab": {
                    "version": "0.0.2",
                    "exportedAt": chrono::Utc::now().timestamp(),
                    "generator": "weflow-server",
                },
                "meta": {
                    "name": store.session_display(&talker),
                    "platform": "wechat",
                    "type": if talker.ends_with("@chatroom") { "group" } else { "private" },
                    "groupId": group_id,
                    "ownerId": owner_id,
                },
                "members": members,
                "messages": msgs,
            })));
        }

        // ---- media export job collection ----
        let mut export_jobs: Vec<(i64, crate::parser::MediaKind, Option<String>, i64, String)> =
            Vec::new();
        if include_media {
            let any_sub = ["image", "tupian", "voice", "vioce", "video", "emoji"]
                .iter()
                .any(|k| crate::server::flex_bool(&params, k));
            let want = |kind: crate::parser::MediaKind| -> bool {
                if !any_sub {
                    return true;
                }
                use crate::parser::MediaKind as K;
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

        let messages: Vec<serde_json::Value> = slice
            .iter()
            .map(|m| message_json(&store, &talker, m, include_media))
            .collect();
        (count, has_more, export_jobs, messages)
    };

    let mut export_jobs = export_jobs;
    let exported: std::collections::HashMap<i64, crate::media::export::ExportedMedia> =
        if export_jobs.is_empty() {
            Default::default()
        } else {
            let account_dir = account.info.dir.clone();
            let export_dir = state.cfg.media_export_dir.clone();
            let mk = account.media_keys.clone();
            let sync = account.sync.clone();
            tokio::task::spawn_blocking(move || {
                sync.lock().export_media_batch(
                    &account_dir,
                    mk,
                    std::path::Path::new(&export_dir),
                    &mut export_jobs,
                    200,
                )
            })
            .await
            .unwrap_or_default()
        };
    let base_url = &state.base_url;
    let mut exported_count = 0usize;
    for mv in &mut messages {
        let Some(local_id) = mv.get("localId").and_then(|v| v.as_i64()) else {
            continue;
        };
        let Some(res) = exported.get(&local_id) else {
            continue;
        };
        if let Some(media) = mv.get_mut("media").and_then(|m| m.as_object_mut()) {
            let url = match &res.external_url {
                Some(u) => u.clone(),
                None => format!(
                    "{base_url}/api/v1/media/{}/{}/{}?access_token={}",
                    talker, res.kind_dir, res.file_name, state.token
                ),
            };
            media.insert("url".into(), json!(url));
            media.insert(
                "localPath".into(),
                json!(res.local_path.to_string_lossy().to_string()),
            );
            media.insert("exported".into(), json!(true));
            exported_count += 1;
        }
    }

    Ok(Json(json!({
        "success": true,
        "talker": talker,
        "count": count,
        "hasMore": has_more,
        "media": { "enabled": include_media, "exportPath": state.cfg.media_export_dir.display().to_string(), "count": exported_count },
        "messages": messages,
    })))
}

