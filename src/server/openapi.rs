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
    crate::server::dto::Page,
    crate::server::dto::PullEnvelope,
    crate::server::dto::PullMessage,
    crate::server::dto::PullSync,
    crate::server::dto::Quote,
    crate::server::dto::SessionChatlab,
    crate::server::dto::SessionNative,
    crate::server::dto::SessionsChatlab,
    crate::server::dto::SessionsNative,
    crate::server::dto::SyncResult,
    crate::server::dto::WatermarkEntry,
    crate::server::dto::WatermarkValue,
)))]
pub struct ApiDoc;

/// 一个端点：路径、方法、成功响应的 schema 名列表（多于一个即 `oneOf`）。
type Shaped = (&'static str, HttpMethod, &'static [&'static str]);

/// 端点表。**它是手写的**：`#[utoipa::path]` 要给每个 handler 加注解，而那些 handler 的
/// 返回类型是 `Response` / `Value`（多形状所致），注解反而容易与真实形状脱节。
///
/// 改路由时**必须**改这里 —— 与 `tests/api_smoke.rs` 的 golden 端点清单互为对照：
/// 那边漏了是快照缺口，这边漏了是描述缺口。
const ENDPOINTS: &[Shaped] = &[
    ("/health", HttpMethod::Get, &["Health"]),
    ("/health", HttpMethod::Post, &["Health"]),
    ("/api/v1/health", HttpMethod::Get, &["Health"]),
    ("/api/v1/health", HttpMethod::Post, &["Health"]),
    ("/api/v1/accounts", HttpMethod::Get, &["AccountsList"]),
    (
        "/api/v1/accounts",
        HttpMethod::Post,
        &["AccountRegistered", "AccountConflict"],
    ),
    (
        "/api/v1/accounts/{wxid}/deregister",
        HttpMethod::Post,
        &["AccountDeregistered", "AccountNotRegistered", "AccountWxidMismatch"],
    ),
    (
        "/api/v1/sessions",
        HttpMethod::Get,
        &["SessionsNative", "SessionsChatlab"],
    ),
    ("/api/v1/sessions/{id}/messages", HttpMethod::Get, &["PullEnvelope"]),
    (
        "/api/v1/messages",
        HttpMethod::Get,
        &["MessagesNative", "MessagesChatlab"],
    ),
    (
        "/api/v1/messages",
        HttpMethod::Post,
        &["MessagesNative", "MessagesChatlab"],
    ),
    ("/api/v1/contacts", HttpMethod::Get, &["Contacts"]),
    ("/api/v1/contacts", HttpMethod::Post, &["Contacts"]),
    ("/api/v1/group-members", HttpMethod::Get, &["GroupMembers"]),
    ("/api/v1/group-members", HttpMethod::Post, &["GroupMembers"]),
    ("/api/v1/sync", HttpMethod::Post, &["SyncResult"]),
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

/// 成功响应：一个 schema 直接用，多个包成 `oneOf`。
fn success_response(names: &[&str]) -> serde_json::Value {
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
    serde_json::json!({
        "description": "成功",
        "content": { "application/json": { "schema": schema } },
    })
}

/// 生成完整描述。
pub fn document() -> OpenApi {
    let base = ApiDoc::openapi();
    let mut paths_json = serde_json::Map::new();
    for (path, method, names) in ENDPOINTS {
        let entry = paths_json
            .entry((*path).to_string())
            .or_insert_with(|| serde_json::json!({}));
        let op_id = format!("{}_{}", method_key(method), path)
            .replace(['/', '{', '}'], "_")
            .trim_matches('_')
            .to_string();
        entry[method_key(method)] = serde_json::json!({
            "operationId": op_id,
            "responses": { "200": success_response(names) },
        });
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