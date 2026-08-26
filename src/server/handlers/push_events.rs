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
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::require_auth;
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_auth(&state, &query, &headers)?;
    let account = {
        let accounts = state.accounts.lock();
        match query.get("wxid").and_then(|w| accounts.get(w).cloned()) {
            Some(a) => a,
            None => accounts
                .values()
                .find(|a| a.status().is_ready())
                .cloned()
                .ok_or_else(|| ApiError::service_unavailable("no ready account"))?,
        }
    };

    // Last-Event-ID replay (header or query param; WeFlow contract)
    let last_id = headers
        .get(HeaderName::from_static("last-event-id"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| query.get("lastEventId").and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0);
    let replay: Vec<(u64, &'static str, serde_json::Value)> =
        account.history.lock().replay_since(last_id);

    let rx = account.events.subscribe();
    let history = account.history.clone();
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
        while let Some(item) = bstream.next().await {
            let ev = match item {
                Ok(ev) => ev,
                Err(_lagged) => {
                    // subscriber fell behind: re-baseline
                    yield Ok(Event::default().event("sync").data("{\"rebased\":true}"));
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

fn serialize_event(ev: crate::sync::Event) -> (&'static str, serde_json::Value) {
    match ev {
        crate::sync::Event::New(m) => (
            "message.new",
            json!({
                "event": "message.new",
                "sessionId": m.session_id,
                "sessionType": m.session_type,
                "rawid": m.rawid,
                "sourceName": m.source_name,
                "groupName": m.group_name,
                "content": m.content,
                "timestamp": m.timestamp,
            }),
        ),
        crate::sync::Event::Revoke(r) => (
            "message.revoke",
            json!({
                "event": "message.revoke",
                "sessionId": r.session_id,
                "sessionType": r.session_type,
                "rawid": r.rawid,
                "sourceName": r.source_name,
                "groupName": r.group_name,
                "content": r.content,
                "timestamp": r.timestamp,
            }),
        ),
        crate::sync::Event::Sync(wms) => (
            "sync",
            json!({
                "event": "sync",
                "watermarks": wms.iter().map(|(k, w)| json!({"table": k, "watermark": {
                    "create_time": w.create_time, "sort_seq": w.sort_seq, "local_id": w.local_id
                }})).collect::<Vec<_>>(),
            }),
        ),
    }
}