//! `/openapi.json` —— 用类型生成机器可读的接口描述。
//!
//! ## 为什么要有它
//!
//! golden 快照钉住的是**当前**响应；schema 描述的是**允许**什么样的响应。两者互补：
//! 快照能发现「你改了」，schema 能说明「改成什么是合法的」。
//!
//! ## 多形状端点用 `oneOf`，不用「可选字段堆叠」
//!
//! `sessions` / `messages` / `accounts` 的响应形状由参数或状态决定。把它们硬塞进一个
//! 「所有字段都可选」的 schema，会让描述**看起来**合法而实际上没有任何一种取值组合是
//! 对的 —— 那比没有 schema 更糟：生成出来的客户端会编译通过、运行时报错。
//!
//! ## 为什么 `paths` 是拼 JSON 再反序列化
//!
//! schema 由 `#[derive(ToSchema)]` 从 DTO 生成（形状只有一个事实源）。而 `paths` 是纯粹
//! 的**文档数据**，不是契约响应 —— 用 builder 逐层构造只会让这段代码变成对其 builder API
//! 的考古。因此按 OpenAPI 规范形状拼好 JSON 再 `from_value`：拼错了在加载时就会失败，
//! 而不是等到有人打开文档才发现。

use utoipa::openapi::path::{HttpMethod, Paths};
use utoipa::OpenApi as _;
use utoipa::openapi::{Components, OpenApi, OpenApiBuilder};

/// 全部响应 schema。新增 DTO 时**必须**在这里登记 —— 漏登记会让接口描述悄悄缺一块。
#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(
    crate::server::AccountPhase,
    crate::server::AccountStateView,
    crate::server::AccountStatus,
    crate::server::dto::AccountConflict,
    crate::server::dto::AccountDeregistered,
    crate::server::dto::AccountNotRegistered,
    crate::server::dto::AccountRegistered,
    crate::server::dto::AccountWxidMismatch,
    crate::server::dto::AccountsList,
    crate::server::dto::ChatlabHeader,
    crate::server::dto::ChatlabMember,
    crate::server::dto::ChatlabMessage,
    crate::server::dto::ChatlabMeta,
    crate::server::dto::Contact,
    crate::server::dto::Contacts,
    crate::server::dto::EventMedia,
    crate::server::dto::EventNew,
    crate::server::dto::EventRevoke,
    crate::server::dto::EventSync,
    crate::server::dto::GroupMember,
    crate::server::dto::GroupMembers,
    crate::server::dto::Health,
    crate::server::dto::MediaEnvelope,
    crate::server::dto::MediaObject,
    crate::server::dto::MessageNative,
    crate::server::dto::MessagesChatlab,
    crate::server::dto::MessagesNative,
    crate::server::dto::NotificationFrame,
    crate::server::dto::Page,
    crate::server::dto::PullEnvelope,
    crate::server::dto::PullMessage,
    crate::server::dto::PullSync,
    crate::server::dto::Quote,
    crate::server::dto::SessionChatlab,
    crate::server::dto::SessionNative,
    crate::server::dto::SessionsChatlab,
    crate::server::dto::SessionsNative,
    crate::server::dto::SyncFrame,
    crate::server::dto::SyncResult,
    crate::server::dto::WatermarkEntry,
    crate::server::dto::WatermarkValue,
)))]
pub struct ApiDoc;

/// 成功响应的形态 —— 不是所有端点都回 JSON。
enum Media {
    /// JSON：schema 名列表（多于一个即 `oneOf`）。
    Json(&'static [&'static str]),
    /// 二进制字节（媒体字节面）。
    Bytes,
    /// SSE 帧流（推送面）。
    EventStream,
    /// 描述文档自身（`/openapi.json`）。
    SelfDoc,
}

/// 一个端点：路径、方法、成功响应的形态、端点级描述。
///
/// `desc` 承载**对外闸门**（导出上限、单页上限、缓冲尺寸）——这些数字只写在散文文档里
/// 就没人查得到，进描述才会随 `/openapi.json` 一起被下游与生成工具消费。
type Ep = (&'static str, HttpMethod, Media, &'static str);

/// 端点表。**它是手写的**：`#[utoipa::path]` 要给每个 handler 加注解，而那些 handler 的
/// 返回类型是 `Response` / `Value`（多形状所致），注解反而容易与真实形状脱节。
///
/// 本表与**真实路由**的对等关系由 `tests/api_smoke.rs` 的
/// `documented_routes_match_the_openapi_table` 强制：路由集合（`server::routes::ROUTES`）
/// 减去豁免集合（`server::routes::NOT_DOCUMENTED`）**必须**等于本表的 (路径, 方法) 集合，
/// 且这里声明的每个方法都要被真实路由接受。豁免写在代码里，不写在注释里。
///
/// 字节面与 SSE 面进不了 golden 端点清单，由本表收录。
const ENDPOINTS: &[Ep] = &[
    ("/health", HttpMethod::Get, Media::Json(&["Health"]), ""),
    ("/health", HttpMethod::Post, Media::Json(&["Health"]), ""),
    ("/api/v1/health", HttpMethod::Get, Media::Json(&["Health"]), ""),
    ("/api/v1/health", HttpMethod::Post, Media::Json(&["Health"]), ""),
    (
        "/openapi.json",
        HttpMethod::Get,
        Media::SelfDoc,
        "免鉴权：只描述形状，不含账号、路径与密钥。",
    ),
    ("/api/v1/accounts", HttpMethod::Get, Media::Json(&["AccountsList"]), ""),
    (
        "/api/v1/accounts",
        HttpMethod::Post,
        Media::Json(&["AccountRegistered", "AccountConflict"]),
        "",
    ),
    (
        "/api/v1/accounts/{wxid}",
        HttpMethod::Delete,
        Media::Json(&["AccountDeregistered", "AccountNotRegistered", "AccountWxidMismatch"]),
        "注销账号；`POST .../{wxid}/deregister` 是其别名（供无法发 DELETE 的客户端与代理）。",
    ),
    (
        "/api/v1/accounts/{wxid}/deregister",
        HttpMethod::Post,
        Media::Json(&["AccountDeregistered", "AccountNotRegistered", "AccountWxidMismatch"]),
        "",
    ),
    (
        "/api/v1/sessions",
        HttpMethod::Get,
        Media::Json(&["SessionsNative", "SessionsChatlab"]),
        "会话列表：`limit` 默认 100。",
    ),
    (
        "/api/v1/sessions",
        HttpMethod::Post,
        Media::Json(&["SessionsNative", "SessionsChatlab"]),
        "同 GET（兼容不能发 GET 的调用方）。",
    ),
    (
        "/api/v1/sessions/{id}/messages",
        HttpMethod::Get,
        Media::Json(&["PullEnvelope"]),
        "游标拉取：`limit` 单页上限 5000。",
    ),
    (
        "/api/v1/messages",
        HttpMethod::Get,
        Media::Json(&["MessagesNative", "MessagesChatlab"]),
        "`media=1` 触发导出，**每请求最多导出 200 项**（超出的保持未导出，再请求续传）。",
    ),
    (
        "/api/v1/messages",
        HttpMethod::Post,
        Media::Json(&["MessagesNative", "MessagesChatlab"]),
        "`media=1` 触发导出，**每请求最多导出 200 项**（超出的保持未导出，再请求续传）。",
    ),
    ("/api/v1/contacts", HttpMethod::Get, Media::Json(&["Contacts"]), ""),
    ("/api/v1/contacts", HttpMethod::Post, Media::Json(&["Contacts"]), ""),
    ("/api/v1/group-members", HttpMethod::Get, Media::Json(&["GroupMembers"]), ""),
    ("/api/v1/group-members", HttpMethod::Post, Media::Json(&["GroupMembers"]), ""),
    (
        "/api/v1/media/{id}",
        HttpMethod::Get,
        Media::Bytes,
        "字节面：只服务导出根下已导出的文件；未导出的先经 `messages?media=1`（每请求上限 200 项）导出。",
    ),
    (
        "/api/v1/media/{id}",
        HttpMethod::Post,
        Media::Bytes,
        "同 GET（兼容不能发 GET 的调用方）。",
    ),
    (
        "/api/v1/media/{talker}/{media_type}/{file}",
        HttpMethod::Get,
        Media::Bytes,
        "三段式字节面；语义同 `/api/v1/media/{id}`。",
    ),
    (
        "/api/v1/media/{talker}/{media_type}/{file}",
        HttpMethod::Post,
        Media::Bytes,
        "同 GET。",
    ),
    (
        "/api/v1/push/messages",
        HttpMethod::Get,
        Media::EventStream,
        "SSE：重放缓冲 1000 条 / 600 秒；广播缓冲 1024；保活 25 秒（注释帧）。载荷为完整事件（老面形状）。",
    ),
    (
        "/api/v1/push/messages",
        HttpMethod::Post,
        Media::EventStream,
        "同 GET。",
    ),
    (
        "/chatlab/push/messages",
        HttpMethod::Get,
        Media::EventStream,
        "SSE 通知面：只发元信息、不发正文；缓冲与保活同老面；基线帧带 `generation`。",
    ),
    (
        "/chatlab/sessions",
        HttpMethod::Get,
        Media::Json(&["SessionsChatlab"]),
        "Pull 形状的发现面：`keyword`/`limit`/`cursor` 分页；`count`/`page` 报告截断。",
    ),
    (
        "/chatlab/sessions/{id}/messages",
        HttpMethod::Get,
        Media::Json(&["PullEnvelope"]),
        "Pull 面（与 `/api/v1/sessions/{id}/messages` 同一实现）：`limit` 单页上限 5000。",
    ),
    ("/api/v1/sync", HttpMethod::Get, Media::Json(&["SyncResult"]), ""),
    ("/api/v1/sync", HttpMethod::Post, Media::Json(&["SyncResult"]), ""),
];

/// 方法名 → OpenAPI 的键。
fn method_key(m: &HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "get",
        HttpMethod::Post => "post",
        HttpMethod::Put => "put",
        HttpMethod::Delete => "delete",
        HttpMethod::Patch => "patch",
        HttpMethod::Head => "head",
        HttpMethod::Options => "options",
        HttpMethod::Trace => "trace",
    }
}

/// 成功响应：JSON 面按 schema 名出（一个直接引用、多个 `oneOf`）；字节面与 SSE 面按各自
/// 的媒体类型出 —— 生成的客户端因此不会拿 JSON 解码器去解字节流或 SSE 帧。
fn success_response(media: &Media) -> serde_json::Value {
    let (content_type, schema) = match media {
        Media::Json(names) => {
            let schema = if names.len() == 1 {
                serde_json::json!({ "$ref": format!("#/components/schemas/{}", names[0]) })
            } else {
                serde_json::json!({
                    "oneOf": names
                        .iter()
                        .map(|n| serde_json::json!({ "$ref": format!("#/components/schemas/{n}") }))
                        .collect::<Vec<_>>(),
                })
            };
            ("application/json", schema)
        }
        Media::Bytes => (
            "application/octet-stream",
            serde_json::json!({ "type": "string", "format": "binary" }),
        ),
        Media::EventStream => (
            "text/event-stream",
            serde_json::json!({
                "type": "string",
                "description": "SSE 帧流（event: + data: 行）。帧结构：老面见 EventNew/EventRevoke/EventSync，通知面见 NotificationFrame/SyncFrame。"
            }),
        ),
        Media::SelfDoc => (
            "application/json",
            serde_json::json!({ "type": "object", "description": "OpenAPI 描述文档自身" }),
        ),
    };
    serde_json::json!({
        "description": "成功",
        "content": { content_type: { "schema": schema } },
    })
}

/// 生成完整描述。
pub fn document() -> OpenApi {
    let base = ApiDoc::openapi();
    let mut paths_json = serde_json::Map::new();
    for (path, method, media, desc) in ENDPOINTS {
        let entry = paths_json
            .entry((*path).to_string())
            .or_insert_with(|| serde_json::json!({}));
        let op_id = format!("{}_{}", method_key(method), path)
            .replace(['/', '{', '}'], "_")
            .trim_matches('_')
            .to_string();
        let mut op = serde_json::json!({
            "operationId": op_id,
            "responses": { "200": success_response(media) },
        });
        if !desc.is_empty() {
            op["description"] = serde_json::Value::String((*desc).to_string());
        }
        entry[method_key(method)] = op;
    }
    let paths: Paths = serde_json::from_value(serde_json::Value::Object(paths_json))
        .expect("端点表必须能构成合法的 OpenAPI paths");
    let components: Components = base.components.clone().unwrap_or_default();
    OpenApiBuilder::new()
        .info(
            utoipa::openapi::InfoBuilder::new()
                .title("weflow-server")
                .version(env!("CARGO_PKG_VERSION"))
                .build(),
        )
        .paths(paths)
        .components(Some(components))
        .build()
}