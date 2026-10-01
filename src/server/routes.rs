//! 路由表 —— **路由的唯一事实源**。
//!
//! 为什么要有它：/openapi.json 的端点表与真实路由此前是两份手写清单，两者之间没有任何
//! 断言盯着。结果是「收口」之后仍有两条真实操作（POST /api/v1/sessions、GET /api/v1/sync）
//! 不在描述里，而提交信息按**路由**条数宣称已经补齐 —— 表、文档、golden 快照同源，
//! 互相印证恒绿，只有读路由代码才看得出来。
//!
//! 现在的约束是结构性的：build_router 由本表构建，所以**不存在「没进表的真实路由」**；
//! 反方向（表里声明了、而路由不接受的方法）由 tests/api_smoke.rs 的
//! documented_routes_match_the_openapi_table 逐方法探测堵住 —— 声明多一个方法、少一个方法
//! 都会红。

use std::sync::Arc;

use axum::routing::{get, post, MethodRouter};

use crate::server::handlers;
use crate::server::AppState;

/// HTTP 方法。它只用于**声明**；真实分派在 method_router，两者由探测测试对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET
    Get,
    /// POST
    Post,
    /// DELETE
    Delete,
}

impl Method {
    /// OpenAPI 里该方法的键名（小写）。
    pub fn key(self) -> &'static str {
        match self {
            Method::Get => "get",
            Method::Post => "post",
            Method::Delete => "delete",
        }
    }
}

/// 处理形状。同名 kind 可以挂在多条路径上（/health 与 /api/v1/health 共用一组 handler）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// /health 与 /api/v1/health（免鉴权，不含身份信息）。
    Health,
    /// /openapi.json（免鉴权：只描述形状，不含账号、路径与密钥）。
    OpenapiJson,
    /// 账号面：列表 + 注册。
    Accounts,
    /// 注销（DELETE）。
    AccountItem,
    /// 注销的别名（供无法发 DELETE 的客户端与代理）。
    AccountDeregister,
    /// 消息面：原生 / ChatLab 两形状由参数切换。
    Messages,
    /// 会话列表（原生 / ChatLab 两形状由参数切换）。
    Sessions,
    /// 游标拉取面（本身即 ChatLab 形状）。
    SessionsPull,
    /// 联系人面。
    Contacts,
    /// 群成员面。
    GroupMembers,
    /// 按导出文件名取字节。
    MediaById,
    /// 三段式取字节（talker/type/file）。
    MediaNamed,
    /// 老面的 SSE 推送。
    PushMessages,
    /// ChatLab 通知面（只发元信息）。
    ChatlabPush,
    /// ChatLab 发现面。
    ChatlabSessions,
    /// ChatLab 拉取面。
    ChatlabPull,
    /// 手动同步。
    Sync,
    /// sns/timeline（**不进接口描述**，见 NOT_DOCUMENTED）。
    SnsTimeline,
    /// sns/usernames
    SnsUsernames,
    /// sns/stats
    SnsStats,
    /// sns/export
    SnsExport,
    /// sns/export/stats
    SnsExportStats,
    /// sns/media/proxy
    SnsMediaProxy,
}

/// 一条路由：路径 + 处理形状 + 接受的方法。
pub struct Route {
    /// axum 路径（占位符用 {name}）。
    pub path: &'static str,
    /// 处理形状。
    pub kind: Kind,
    /// 本路由接受的方法。**必须与 method_router 的分派一致** —— 由探测测试保证。
    pub methods: &'static [Method],
}

const GET_POST: &[Method] = &[Method::Get, Method::Post];
const GET_ONLY: &[Method] = &[Method::Get];
const POST_ONLY: &[Method] = &[Method::Post];
const DELETE_ONLY: &[Method] = &[Method::Delete];

/// 全部路由。**加一条路由只改这里** —— 不在这里注册的路由根本不存在。
///
/// 顺序与历史一致，便于与提交历史对照；它不影响匹配（axum 自己按具体度排序）。
pub const ROUTES: &[Route] = &[
    Route { path: "/health", kind: Kind::Health, methods: GET_POST },
    Route { path: "/api/v1/health", kind: Kind::Health, methods: GET_POST },
    // 接口描述**免鉴权**：它描述的是形状，不含任何本机信息（账号、路径、密钥都不在里面），
    // 而且正是给尚未拿到 token 的接入方看的。
    Route { path: "/openapi.json", kind: Kind::OpenapiJson, methods: GET_ONLY },
    Route { path: "/api/v1/accounts", kind: Kind::Accounts, methods: GET_POST },
    Route { path: "/api/v1/accounts/{wxid}", kind: Kind::AccountItem, methods: DELETE_ONLY },
    Route {
        path: "/api/v1/accounts/{wxid}/deregister",
        kind: Kind::AccountDeregister,
        methods: POST_ONLY,
    },
    Route { path: "/api/v1/messages", kind: Kind::Messages, methods: GET_POST },
    Route { path: "/api/v1/sessions", kind: Kind::Sessions, methods: GET_POST },
    Route { path: "/api/v1/sessions/{id}/messages", kind: Kind::SessionsPull, methods: GET_ONLY },
    Route { path: "/api/v1/contacts", kind: Kind::Contacts, methods: GET_POST },
    Route { path: "/api/v1/group-members", kind: Kind::GroupMembers, methods: GET_POST },
    Route { path: "/api/v1/media/{id}", kind: Kind::MediaById, methods: GET_POST },
    Route {
        path: "/api/v1/media/{talker}/{media_type}/{file}",
        kind: Kind::MediaNamed,
        methods: GET_POST,
    },
    Route { path: "/api/v1/push/messages", kind: Kind::PushMessages, methods: GET_POST },
    // ── ChatLab 适配面（新增，**不改老路由**）──────────────────────────────
    //
    // 规范把 baseUrl 定义为 /chatlab，于是这三条是 Pull 形状的入口。它们与 /api/v1/*
    // **共用同一份实现与同一条总线**，差别只在默认语义：老面靠 format=chatlab 参数切换，
    // 新面**天生就是** ChatLab 形状（调用方不必知道还有另一种）。
    Route { path: "/chatlab/push/messages", kind: Kind::ChatlabPush, methods: GET_ONLY },
    Route { path: "/chatlab/sessions", kind: Kind::ChatlabSessions, methods: GET_ONLY },
    // Pull 面**本身就是** ChatLab 形状（它没有 format 参数）。挂到规范约定的
    // {baseUrl}/sessions/{id}/messages 上，于是 baseUrl=/chatlab 三条路由齐了。
    Route {
        path: "/chatlab/sessions/{id}/messages",
        kind: Kind::ChatlabPull,
        methods: GET_ONLY,
    },
    Route { path: "/api/v1/sync", kind: Kind::Sync, methods: GET_POST },
    Route { path: "/api/v1/sns/timeline", kind: Kind::SnsTimeline, methods: GET_POST },
    Route { path: "/api/v1/sns/usernames", kind: Kind::SnsUsernames, methods: GET_POST },
    Route { path: "/api/v1/sns/stats", kind: Kind::SnsStats, methods: GET_POST },
    Route { path: "/api/v1/sns/export", kind: Kind::SnsExport, methods: GET_POST },
    Route { path: "/api/v1/sns/export/stats", kind: Kind::SnsExportStats, methods: GET_POST },
    Route { path: "/api/v1/sns/media/proxy", kind: Kind::SnsMediaProxy, methods: GET_POST },
];

/// **只有路由、不进 /openapi.json** 的那些。
///
/// sns/* 是强绑定 XML 形状的朋友圈面，DTO 化排在最后一批，因此它们没有 schema 可引用。
/// 豁免**写在这里而不是注释里**：对等测试用它做集合差 —— 漏写一条豁免、或豁免了一条
/// 其实有描述的路径，都会红。
pub const NOT_DOCUMENTED: &[Kind] = &[
    Kind::SnsTimeline,
    Kind::SnsUsernames,
    Kind::SnsStats,
    Kind::SnsExport,
    Kind::SnsExportStats,
    Kind::SnsMediaProxy,
];

/// kind → axum 的 MethodRouter。
///
/// 这里是**唯一的真实分派点**：ROUTES 提供路径与声明，本函数把它翻成 axum 的方法路由。
/// 声明与分派一旦不一致（例如声明了 POST 而这里只 get），探测测试立刻变红。
pub fn method_router(kind: Kind) -> MethodRouter<Arc<AppState>> {
    use handlers::*;
    match kind {
        Kind::Health => get(health::handler).post(health::handler),
        Kind::OpenapiJson => get(crate::server::openapi_handler),
        Kind::Accounts => get(accounts::list_handler).post(accounts::handler),
        Kind::AccountItem => axum::routing::delete(accounts::delete_handler),
        Kind::AccountDeregister => post(accounts::delete_handler),
        Kind::Messages => get(messages::handler).post(messages::handler),
        Kind::Sessions => get(sessions::handler).post(sessions::handler),
        Kind::SessionsPull => get(chatlab_pull::handler),
        Kind::Contacts => get(contacts::handler).post(contacts::handler),
        Kind::GroupMembers => get(group_members::handler).post(group_members::handler),
        Kind::MediaById => get(media::handler_by_id).post(media::handler_by_id),
        Kind::MediaNamed => get(media::handler).post(media::handler),
        Kind::PushMessages => get(push_events::handler).post(push_events::handler),
        Kind::ChatlabPush => get(chatlab_push::handler),
        Kind::ChatlabSessions => get(chatlab_sessions::handler),
        Kind::ChatlabPull => get(chatlab_pull::handler),
        Kind::Sync => get(sync::handler).post(sync::handler),
        Kind::SnsTimeline => get(sns::timeline).post(sns::timeline),
        Kind::SnsUsernames => get(sns::usernames).post(sns::usernames),
        Kind::SnsStats => get(sns::stats).post(sns::stats),
        Kind::SnsExport => get(sns::export).post(sns::export),
        Kind::SnsExportStats => get(sns::export_stats).post(sns::export_stats),
        Kind::SnsMediaProxy => get(sns::media_proxy).post(sns::media_proxy),
    }
}
