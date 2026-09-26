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
    let replay: Vec<(u64, &'static str, serde_json::Value)> =
        state.history.lock().replay_since(last_id);

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
        for (id, name, payload) in replay {
            yield Ok(Event::default()
                .id(id.to_string())
                .event(name)
                .json_data(payload)
                .unwrap_or_else(|_| Event::default().event("message.new").data("{}")));
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
                    let (name, payload) = serialize_event(crate::sync::Event::Sync(wms));
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
            let (name, payload) = serialize_event(ev);
            let id = history.lock().append(name, payload.clone());
            yield Ok(Event::default()
                .id(id.to_string())
                .event(name)
                .json_data(payload)
                .unwrap_or_else(|_| Event::default().event("message.new").data("{}")));
        }
    });

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)).text("ping"))
        .into_response())
}

/// 把内部事件映射成线上载荷。
///
/// 构造 DTO 后 `to_value`：`json!` 走 BTreeMap 会把键排序，`to_value` 同样如此，
/// 因此**输出逐字节不变**；而类型化构造让「键名写错」变成编译错误 —— SSE 是流式接口，
/// 没有快照护栏，这一层就是它的护栏。
fn serialize_event(ev: crate::sync::Event) -> (&'static str, serde_json::Value) {
    match ev {
        crate::sync::Event::New(m) => (
            "message.new",
            serde_json::to_value(EventNew {
                content: m.content,
                event: "message.new".to_string(),
                group_name: m.group_name,
                media: m.media.as_ref().map(|md| EventMedia {
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
