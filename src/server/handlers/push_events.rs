//! GET/POST /api/v1/push/messages — SSE stream of message events.
//!
//! WeFlow contract: `ready` first, then `message.new` / `message.revoke` with
//! `id:` frames, Last-Event-ID replay (1000 events / 10 min TTL), 25s
//! keep-alive ping.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::server::dto::{EventMedia, EventNew, EventRevoke, EventSync, WatermarkEntry, WatermarkValue};
use crate::server::error::ApiResult;
use crate::server::handlers::require_auth;
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_auth(&state, &query, &headers)?;

    // No readiness gate (qqflow-server parity): the bus and replay history are
    // process-wide, so a client may connect before any account is registered
    // and starts receiving events once indexing completes. Gating here forced
    // downstream clients into a reconnect-backoff loop through the whole cold
    // start, and — with the bus previously living per account — replacing an
    // `error` account silently orphaned every live subscriber.
    //
    // Last-Event-ID replay (header or query param; WeFlow contract)
    let last_id = headers
        .get(HeaderName::from_static("last-event-id"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| query.get("lastEventId").and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0);
    let export_dir = state.cfg.media_export_dir.clone();
    Ok(sse_from(state, last_id, &export_dir, serialize_event))
}

/// 事件的序列化器：把一个总线事件变成一个 SSE 帧（`(事件名, 载荷)`）。
///
/// 两个面对同一批事件有**不同的形状要求** —— WeFlow 兼容面发完整消息，ChatLab 面只发元信息
/// （规范：「ChatLab 不假设事件可靠送达」，通知只负责告诉客户端去拉）。参数化这一处，
/// 其余（鉴权、重放、保活、滞后重基线、关机）两面对**完全一致**。
///
/// `export_dir` 是**取字节能力的判据输入**：`mediaId` 只在导出根下确有文件时才通告
/// （见 `media::export::fetchable_media_id`）。它随流一起传，而不是每次现查配置。
pub(crate) type Serializer =
    fn(crate::sync::Event, &std::path::Path) -> (&'static str, serde_json::Value);

/// 组装 SSE 响应。`serialize` 决定帧的形状，其余部分是两面的公共部分。
pub(crate) fn sse_from(
    state: Arc<AppState>,
    last_id: u64,
    export_dir: &std::path::Path,
    serialize: Serializer,
) -> Response {
    let export_dir = export_dir.to_path_buf();
    let replay = state.history.lock().replay_since(last_id);
    let rx = state.events.subscribe();
    let history = state.history.clone();
    // An SSE stream never ends on its own, so it would hold graceful shutdown
    // open for the whole grace period. Watching the shutdown channel lets the
    // stream close itself and the drain finish promptly.
    let mut shutdown = state.shutdown.subscribe();
    let lag_state = state.clone();
    let stream = async_stream::stream!({
        yield Ok::<_, std::convert::Infallible>(
            Event::default().event("ready").data("{\"status\":\"ok\"}"),
        );
        for (id, ev) in replay {
            let (name, payload) = serialize(ev, &export_dir);
            yield Ok(Event::default()
                .id(id.to_string())
                .event(name)
                .json_data(payload)
                .unwrap_or_else(|_| Event::default().event("message.new").data("{}")));
        }
        // **连接基线**：把当前水位先给出去。
        //
        // 没有它，客户端在「连接建立」到「第一次水位变化」之间是**盲的** —— 而这中间可能很长
        // （账号空闲、或还没注册账号）。它也让「重放里没有 sync」与「sync 就是空水位」不再混淆。
        //
        // 必须走 `serialize`，不能直接吐 DTO：通知面与老面的 `sync` 形状不同，直接吐会让新面
        // 收到另一个面的形状（这个错在只连新面时看不出来）。
        {
            let (name, payload) = serialize(crate::sync::Event::Sync(crate::server::current_watermarks(&state)), &export_dir);
            yield Ok(Event::default()
                .event(name)
                .json_data(payload)
                .unwrap_or_else(|_| Event::default().event("sync").data("{}")));
        }
        let mut bstream = BroadcastStream::new(rx);
        loop {
            let item = tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                item = bstream.next() => match item {
                    Some(item) => item,
                    None => break,
                },
            };
            let ev = match item {
                Ok(ev) => ev,
                Err(_lagged) => {
                    // Subscriber fell behind. Re-baseline with the CURRENT
                    // watermarks: a bare `{"rebased":true}` tells the client
                    // it lost events but gives it nothing to resync from.
                    let wms = crate::server::current_watermarks(&lag_state);
                    let (name, payload) = serialize(crate::sync::Event::Sync(wms), &export_dir);
                    // No history id: this frame is specific to this lagging
                    // subscriber, so it must not consume a bus-level
                    // sequence number that other clients would then skip.
                    yield Ok(Event::default()
                        .event(name)
                        .json_data(payload)
                        .unwrap_or_else(|_| Event::default().event("sync").data("{}")));
                    continue;
                }
            };
            let id = history.lock().append(ev.clone());
            let (name, payload) = serialize(ev, &export_dir);
            yield Ok(Event::default()
                .id(id.to_string())
                .event(name)
                .json_data(payload)
                .unwrap_or_else(|_| Event::default().event("message.new").data("{}")));
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)).text("ping"))
        .into_response()
}

/// 把内部事件映射成线上载荷。
///
/// 构造 DTO 后 `to_value`：`json!` 走 BTreeMap 会把键排序，`to_value` 同样如此，
/// 因此**输出逐字节不变**；而类型化构造让「键名写错」变成编译错误 —— SSE 是流式接口，
/// 没有快照护栏，这一层就是它的护栏。
fn serialize_event(
    ev: crate::sync::Event,
    export_dir: &std::path::Path,
) -> (&'static str, serde_json::Value) {
    match ev {
        crate::sync::Event::New(m) => (
            "message.new",
            serde_json::to_value(EventNew {
                content: m.content,
                event: "message.new".to_string(),
                group_name: m.group_name,
                media: m.media.as_ref().map(|md| EventMedia {
                    // 「出现即可取」：只有导出根下确有这个文件才通告 id（见
                    // `media::export::fetchable_media_id`）。
                    media_id: md.kind_dir.and_then(|dir| {
                        crate::media::export::fetchable_media_id(
                            export_dir,
                            &m.session_id,
                            dir,
                            &md.file_name,
                        )
                    }),
                    file_name: md.file_name.clone(),
                    md5: md.md5.clone(),
                    r#type: md.kind.to_string(),
                }),
                rawid: m.rawid,
                session_id: m.session_id,
                session_type: m.session_type.to_string(),
                source_name: m.source_name,
                timestamp: m.timestamp,
            })
            .expect("事件载荷必须可序列化"),
        ),
        crate::sync::Event::Revoke(r) => (
            "message.revoke",
            serde_json::to_value(EventRevoke {
                content: r.content,
                event: "message.revoke".to_string(),
                group_name: r.group_name,
                rawid: r.rawid,
                session_id: r.session_id,
                session_type: r.session_type.to_string(),
                source_name: r.source_name,
                timestamp: r.timestamp,
            })
            .expect("事件载荷必须可序列化"),
        ),
        crate::sync::Event::Sync(wms) => (
            "sync",
            serde_json::to_value(EventSync {
                event: "sync".to_string(),
                generation: crate::server::current_generation(),
                watermarks: wms
                    .iter()
                    .map(|(k, w)| WatermarkEntry {
                        table: k.to_string(),
                        watermark: WatermarkValue {
                            create_time: w.create_time,
                            local_id: w.local_id,
                            sort_seq: w.sort_seq,
                        },
                    })
                    .collect(),
            })
            .expect("事件载荷必须可序列化"),
        ),
    }
}
