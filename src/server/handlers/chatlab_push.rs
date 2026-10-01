//! `GET /chatlab/push/messages` —— **通知面**。
//!
//! 与 `/api/v1/push/messages` 是**同一条总线、同一套连接机制**（鉴权、`Last-Event-ID` 重放、
//! 保活、开机重基线、关机自收），差别只有**帧的形状**：
//!
//! | | `/api/v1/push/messages` | `/chatlab/push/messages` |
//! |---|---|---|
//! | 载荷 | 完整消息（含正文与媒体元数据）| **只带元信息** |
//! | 定位 | WeFlow 兼容面 —— 已有客户端在解析它 | 规范里的通知通道 |
//!
//! 规范对这条通道的定位是「**仅通知**：ChatLab 不假设 SSE 事件可靠送达」——收到事件后**去拉**
//! 那一页，而不是把事件内容当数据。所以这里不带正文：带了会诱导客户端把它当数据源，而它并不
//! 保证送达。
//!
//! 老面**不动**（「不改老路由」）：它的形状是既有客户端在解析的，改它要走破坏性发布。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName};
use axum::response::Response;

use crate::server::error::ApiResult;
use crate::server::handlers::require_auth;
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_auth(&state, &query, &headers)?;
    let last_id = headers
        .get(HeaderName::from_static("last-event-id"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| query.get("lastEventId").and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0);
    // 与老面共用整套连接机制，只换序列化器。导出根是**取字节能力的判据输入**（通知帧目前不用它，
    // 但签名与老面一致 —— 两面走同一条流，参数不能各写一套）。
    let export_dir = state.cfg.media_export_dir.clone();
    Ok(super::push_events::sse_from(
        state,
        last_id,
        &export_dir,
        serialize_notification,
    ))
}

/// 把一个总线事件映射成**通知帧**：只带标识与时间，不带正文。
///
/// - `message.new` → `{eventId, sessionId, timestamp, platformMessageId?, event}`
/// - `message.revoke` → 同形（撤回也是一种「有新情况，去拉」的通知）
/// - `sync` → `{generation, watermarks:[…]}`（基线；`generation` 的用途见 `server::GENERATION`）
fn serialize_notification(
    ev: crate::sync::Event,
    _export_dir: &std::path::Path,
) -> (&'static str, serde_json::Value) {
    use crate::server::dto::{NotificationFrame, SyncFrame};
    match ev {
        crate::sync::Event::Sync(wms) => (
            "sync",
            serde_json::to_value(SyncFrame {
                event: "sync".to_string(),
                generation: crate::server::current_generation(),
                watermarks: wms
                    .into_iter()
                    .map(|(table, w)| crate::server::dto::WatermarkEntry {
                        table,
                        watermark: crate::server::dto::WatermarkValue {
                            create_time: w.create_time,
                            local_id: w.local_id,
                            sort_seq: w.sort_seq,
                        },
                    })
                    .collect(),
            })
            .expect("通知帧必须可序列化"),
        ),
        crate::sync::Event::New(m) => (
            "message.new",
            serde_json::to_value(NotificationFrame {
                event: "message.new".to_string(),
                event_id: m.rawid,
                platform_message_id: None,
                session_id: m.session_id,
                timestamp: m.timestamp,
            })
            .expect("通知帧必须可序列化"),
        ),
        crate::sync::Event::Revoke(r) => (
            "message.revoke",
            serde_json::to_value(NotificationFrame {
                event: "message.revoke".to_string(),
                event_id: r.rawid.clone(),
                // 撤回帧**带上平台消息号**：本仓事件里的 rawid 就是平台号（与 eventId 同值，
                // 但不是同一件事 —— 前者是那条消息的身份，后者是事件通道的身份）。拉取面用它
                // 定位被撤回的那条；不给的话客户端只能靠时间戳去猜。
                platform_message_id: Some(r.rawid),
                session_id: r.session_id,
                timestamp: r.timestamp,
            })
            .expect("通知帧必须可序列化"),
        ),
    }
}
