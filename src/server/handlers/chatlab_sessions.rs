//! `GET /chatlab/sessions` —— Pull 形状的**发现面**。
//!
//! 规范把 `baseUrl` 定义为 `/chatlab`，这条是其中的会话发现入口。与 `/api/v1/sessions`
//! **共用同一份实现**（`sessions::respond`），差别只有默认语义：老面靠 `format=chatlab` 参数
//! 切换，新面**天生就是** ChatLab 形状 —— 调用方不必知道还有另一种。**老面一行未改。**
//!
//! 响应形状（规范）：
//!
//! ```json
//! { "count": 2,
//!   "sessions": [ { "id", "name", "platform", "type", "messageCount", "memberCount?", "lastMessageAt" } ],
//!   "page": { "hasMore": true, "nextCursor": "…" } }
//! ```
//!
//! `page` 是**可选增强**：规范说客户端在响应里**未发现** `page` 时按「单次全量结果」处理。
//! 这里总是给 `page` —— 那比「靠条数猜有没有截断」明确，而契约套件里有一条断言正是查这个
//! （没有 `page` 时返回数不得超过 `limit`）。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;

use crate::server::error::ApiResult;
use crate::server::AppState;

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<axum::response::Response> {
    // `force_chatlab = true`：Pull 面不该要求调用方传 `format=chatlab`。
    super::sessions::respond(&state, &query, &headers, body, true).await
}
