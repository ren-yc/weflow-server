//! MCP 工具面（mcp feature）：把本服务的只读查询暴露成 MCP 工具。
//!
//! 为什么是独立进程加 HTTP 客户端：MCP 进程不碰数据库密钥，也不重复实现解密与索引，只持有
//! API token 并经 SDK 打本机 HTTP 面（同 CLI 的理由 —— 两份取数实现必然漂移，而只有 SDK
//! 那一份被契约测试钉住）。代价是先跑服务端。
//!
//! 数据会离开本机：工具输出进入模型上下文。这条提示写进 get_info 的 instructions 与每个
//! 工具的 description，因为模型看不到 README。
//!
//! 失败通道分两种。「工具跑成了但没有结果」（会话不存在、查无结果）走
//! Ok(CallToolResult::error(..))，调用方看得到说明；「服务端根本没法处理」（缺 token、
//! 传输层失败）走 Err(ErrorData)，多数客户端只渲染成一句笼统的内部错误。

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use weflow_client::client::{Client, ClientError, ContactsQuery, MessageQuery};

/// 单次工具输出的字符预算（约 32 KB）：超了就少给几条并置 truncated。
const CHAR_BUDGET: usize = 32 * 1024;
/// 「取一页」类工具的默认与上限条数。
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

/// 数据离机提示：同时出现在 instructions 与每个工具的 description 里。
const OFF_MACHINE: &str = "输出会进入模型上下文（对话内容离开本机）。";

const CHATLAB_MESSAGE_KEYS: &[&str] = &[
    "platformMessageId", "timestamp", "sender", "accountName", "groupNickname",
    "type", "content", "replyToMessageId", "media",
];
const NATIVE_MESSAGE_KEYS: &[&str] = &[
    "serverId", "createTime", "senderName", "senderUsername", "content", "rawContent",
    "isSend", "localType", "replyToMessageId", "media",
];

/// 入口：在 stdio 上跑 MCP 服务，直到对端关闭。
pub(crate) fn run(base_url: String, token: String) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("建 tokio 运行时失败")?;
    rt.block_on(async move {
        let service = WeflowMcp::new(base_url, token);
        let running = service
            .serve(rmcp::transport::io::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("MCP 初始化失败: {e}"))?;
        running.waiting().await?;
        Ok(())
    })
}

/// 工具面的持有者：一个指向本机服务的 SDK 客户端。
struct WeflowMcp {
    client: Client,
    base_url: String,
}

impl WeflowMcp {
    fn new(base_url: String, token: String) -> Self {
        Self { client: Client::new(base_url.clone(), token), base_url }
    }
}

/// Debug 手写：Client 里握着 API token，而 MCP 的报错常被 agent 原样贴进日志。
impl fmt::Debug for WeflowMcp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeflowMcp")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

// ---- 参数 ------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ListSessionsArgs {
    /// 会话名/备注的关键词（服务端大小写不敏感过滤）
    keyword: Option<String>,
    /// 最多返回多少条（默认 50，上限 200）
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetMessagesArgs {
    /// 会话 ID（…@chatroom 为群）
    talker: String,
    /// 起始时间：unix 秒 / YYYYMMDD / 7d / 24h（含边界）
    since: Option<String>,
    /// 单页条数（默认 50，上限 200）
    limit: Option<usize>,
    /// 同一时间组内的游标：翻页时原样回传上一次的 nextOffset
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetRawMessagesArgs {
    /// 会话 ID
    talker: String,
    /// 关键词子串
    keyword: Option<String>,
    /// 起始时间（unix 秒或 YYYYMMDD，含边界）
    start: Option<String>,
    /// 结束时间（unix 秒或 YYYYMMDD；裸日期覆盖整天）
    end: Option<String>,
    /// 单页条数（默认 50，上限 200）
    limit: Option<usize>,
    /// 偏移游标：按上一页实际返回条数推进
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchMessagesArgs {
    /// 会话 ID
    talker: String,
    /// 关键词（必填）
    keyword: String,
    /// 单页条数（默认 50，上限 200）
    limit: Option<usize>,
    /// 翻页游标：原样回传上一次的 `nextOffset`
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetContactsArgs {
    /// 关键词（服务端过滤）
    keyword: Option<String>,
    /// 单页条数（默认 50，上限 200）
    limit: Option<usize>,
    /// 偏移游标
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetMediaArgs {
    /// 单段句柄：原生面的 mediaId，或 media.url 的末段
    handle: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GroupMembersArgs {
    /// 群会话 ID（…@chatroom）
    chatroom: String,
    /// 是否统计真实消息数（默认 false：计数是全会话扫描，不是免费的）
    include_message_counts: Option<bool>,
}

// ---- 纯函数 ------------------------------------------------------------------

/// 把 SDK 错误分流到两条通道：404 是「工具跑成了、但没有这个东西」，其余是服务端不可用。
fn sdk_outcome(e: ClientError) -> Result<CallToolResult, McpError> {
    if let ClientError::Status { status: 404, .. } = e {
        Ok(tool_error(format!("{e}")))
    } else {
        Err(McpError::internal_error(format!("{e}"), None))
    }
}

fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn clamp_limit(v: Option<usize>) -> usize {
    v.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// 按 key 白名单投影一个 JSON 对象；缺失与 null 的键一律省略。
fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut map = Map::new();
    for k in keys {
        if let Some(x) = v.get(*k)
            && !x.is_null()
        {
            map.insert((*k).to_string(), x.clone());
        }
    }
    Value::Object(map)
}

fn project_list(items: &[Value], keys: &[&str]) -> Vec<Value> {
    items.iter().map(|v| pick(v, keys)).collect()
}

struct Projected {
    items: Vec<Value>,
    truncated: bool,
}

/// 按条数与字符预算裁剪。第一条永远保留：否则一条长消息会得到「既无内容又无截断标记」的
/// 结果，那是 agent 场景里最坏的一种失败。
fn project_messages(items: Vec<Value>, max_items: usize, budget: usize) -> Projected {
    let total = items.len();
    let mut out: Vec<Value> = Vec::new();
    let mut used = 0usize;
    for item in items.into_iter().take(max_items) {
        let size = serde_json::to_string(&item).map(|s| s.chars().count()).unwrap_or(0);
        if !out.is_empty() && used + size > budget {
            break;
        }
        used += size;
        out.push(item);
    }
    Projected { truncated: out.len() < total, items: out }
}

/// 解析时间参数为 unix 秒：unix 秒 / YYYYMMDD（当天 00:00 UTC）/ 7d / 24h（相对 now）。
/// now 由调用方传入，纯函数才可被直接测。
fn parse_since(spec: &str, now: i64) -> std::result::Result<i64, String> {
    let s = spec.trim();
    if let Some(rest) = s.strip_suffix('d') {
        let n: i64 = rest.parse().map_err(|_| format!("{spec} 不是合法的天数（形如 7d）"))?;
        return Ok(now - n * 86_400);
    }
    if let Some(rest) = s.strip_suffix('h') {
        let n: i64 = rest.parse().map_err(|_| format!("{spec} 不是合法的小时数（形如 24h）"))?;
        return Ok(now - n * 3_600);
    }
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i32 = s[0..4].parse().map_err(|_| format!("{spec} 年份非法"))?;
        let m: u32 = s[4..6].parse().map_err(|_| format!("{spec} 月份非法"))?;
        let d: u32 = s[6..8].parse().map_err(|_| format!("{spec} 日非法"))?;
        let date = chrono::NaiveDate::from_ymd_opt(y, m, d)
            .ok_or_else(|| format!("{spec} 不是合法日期"))?;
        return Ok(date.and_time(chrono::NaiveTime::MIN).and_utc().timestamp());
    }
    s.parse::<i64>()
        .map_err(|_| format!("{spec} 需为 unix 秒、YYYYMMDD、7d 或 24h"))
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---- 工具 --------------------------------------------------------------------

#[tool_router]
impl WeflowMcp {
    #[tool(description = "列出会话（可选关键词）。给会话定位用。输出会离开本机。", annotations(read_only_hint = true))]
    async fn list_sessions(
        &self,
        Parameters(a): Parameters<ListSessionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(a.limit);
        let all = match self.client.list_all_sessions(Some(10_000), a.keyword.as_deref()).await {
            Ok(v) => v,
            Err(e) => return sdk_outcome(e),
        };
        let values: Vec<Value> = all
            .iter()
            .map(|s| {
                json!({
                    "username": s.username,
                    "displayName": s.display_name,
                    "sessionType": s.session_type,
                    "lastTimestamp": s.last_timestamp,
                    "messageCount": s.message_count,
                    "unreadCount": s.unread_count,
                })
            })
            .collect();
        let proj = project_messages(values, limit, CHAR_BUDGET);
        Ok(CallToolResult::structured(json!({
            "count": proj.items.len(),
            "truncated": proj.truncated,
            "sessions": proj.items,
        })))
    }

    #[tool(description = "取一个会话的消息（ChatLab 形状，时间升序）。输出会离开本机。", annotations(read_only_hint = true))]
    async fn get_messages(
        &self,
        Parameters(a): Parameters<GetMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(a.limit);
        let since = match a.since.as_deref().map(|s| parse_since(s, now_secs())).transpose() {
            Ok(v) => v,
            Err(e) => return Ok(tool_error(e)),
        };
        let page = match self
            .client
            .pull_page(&a.talker, since, a.offset.unwrap_or(0), Some(limit as u32))
            .await
        {
            Ok(p) => p,
            Err(e) => return sdk_outcome(e),
        };
        let raw: Vec<Value> =
            page.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        let page_size = raw.len();
        let proj = project_messages(project_list(&raw, CHATLAB_MESSAGE_KEYS), limit, CHAR_BUDGET);
        // 本页被预算截短时不能给出整页游标：Pull 的 nextSince 是整页最后一条的时间，用它
        // 续拉会跳过我们没给出去的那些条。
        let cut = proj.truncated;
        let start = a.offset.unwrap_or(0);
        // since 是调用方的相对串（"7d"/"24h"），解析出的**绝对下界**必须原样回给：
        // 续拉发生在下一轮对话，「现在」已经前移，重发相对串会把窗口悄悄向前挪，
        // 中间那段时间的消息就被跳过了。
        let since_resolved = since;
        Ok(CallToolResult::structured(json!({
            "talker": a.talker,
            "count": proj.items.len(),
            "pageSize": page_size,
            "truncated": cut,
            // 预算截断时**必须**为真：只置 `truncated` 而 `hasMore=false`，按 hasMore 判停的调用方
            // 会把「被预算砍掉的条」当成「没有了」。
            "hasMore": page.sync.has_more || cut,
            // 截断时**不**给 `nextSince`（它指整页末条，会跳过没给出的条），但**给** `nextOffset`：
            // 这一面从 `offset` 起是连续切片，所以 `start + 给出条数` 恰指向被砍掉的第一条 —— 否则
            // 一个纯按字段续拉的调用方会永远重取同一页。
            "nextSince": if cut { Value::Null } else { json!(page.sync.next_since) },
            "nextOffset": if cut {
                json!(start + proj.items.len() as u64)
            } else {
                json!(page.sync.next_offset)
            },
            // 绝对下界：续拉时传它而不是原始 since 串（下一轮 now 已前移，相对串会挪窗）。
            "sinceResolved": since_resolved,
            "hint": if cut { json!("本页超过字符预算，已少给若干条：用 nextOffset 且 since 传响应里的 sinceResolved（绝对下界，不要重发相对串——下一轮 now 已前移会挪窗）续拉；也可用更小的 limit 重取本页，此时按 platformMessageId 去重") } else { Value::Null },
            "messages": proj.items,
        })))
    }

    #[tool(description = "取一个会话的消息，原生形状：带 rawContent、isSend、localType，可选媒体元数据。输出会离开本机。", annotations(read_only_hint = true))]
    async fn get_messages_raw(
        &self,
        Parameters(a): Parameters<GetRawMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(a.limit);
        let mut q = MessageQuery::new(a.talker.clone());
        q.keyword = a.keyword.clone();
        q.start = a.start.clone();
        q.end = a.end.clone();
        q.limit = Some(limit as u32);
        q.offset = a.offset;
        let page = match self.client.list_messages(&q).await {
            Ok(p) => p,
            Err(e) => return sdk_outcome(e),
        };
        let raw: Vec<Value> =
            page.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        let proj = project_messages(project_list(&raw, NATIVE_MESSAGE_KEYS), limit, CHAR_BUDGET);
        // 原生面按 offset 翻页且本页是连续切片：砍掉尾部后，nextOffset 指向被砍掉的第一条。
        let next_offset = a.offset.unwrap_or(0) + proj.items.len() as u64;
        Ok(CallToolResult::structured(json!({
            "talker": a.talker,
            "count": proj.items.len(),
            "truncated": proj.truncated,
            "hasMore": page.has_more || proj.truncated,
            "nextOffset": next_offset,
            "messages": proj.items,
        })))
    }

    #[tool(description = "在一个会话内按关键词搜消息（ChatLab 形状）。输出会离开本机。", annotations(read_only_hint = true))]
    async fn search_messages(
        &self,
        Parameters(a): Parameters<SearchMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(a.limit);
        let start = a.offset.unwrap_or(0);
        let mut q = MessageQuery::new(a.talker.clone());
        q.keyword = Some(a.keyword.clone());
        q.limit = Some(limit as u32);
        q.offset = Some(start);
        let page = match self.client.chatlab_messages(&q).await {
            Ok(p) => p,
            Err(e) => return sdk_outcome(e),
        };
        let raw: Vec<Value> =
            page.messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        let proj = project_messages(project_list(&raw, CHATLAB_MESSAGE_KEYS), limit, CHAR_BUDGET);
        // **给出调用方真能用的游标**：这一面按 `offset` 翻页且本页是连续切片，所以「下一个偏移」
        // 就是 `start + 实际给出条数`（预算截断时它恰好指向被砍掉的第一条）。
        //
        // 刻意**不返回** `page.nextCursor`：ChatLab 的 cursor 只能经 `cursor=` 回传，而这个工具的参数
        // 与 SDK 的查询结构都没有 cursor 字段 —— 给一个回传不了的游标，等于告诉调用方「翻页可用」，
        // 而实际上第 2 页永远取不到。
        Ok(CallToolResult::structured(json!({
            "talker": a.talker,
            "keyword": a.keyword,
            "count": proj.items.len(),
            "truncated": proj.truncated,
            "hasMore": page.page.has_more || proj.truncated,
            "nextOffset": start + proj.items.len() as u64,
            "messages": proj.items,
        })))
    }

    #[tool(description = "联系人列表（备注/昵称/别名只在这个面）。输出会离开本机。", annotations(read_only_hint = true))]
    async fn get_contacts(
        &self,
        Parameters(a): Parameters<GetContactsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = clamp_limit(a.limit);
        let start = a.offset.unwrap_or(0);
        let q = ContactsQuery {
            limit: Some(limit as u32),
            offset: a.offset,
            keyword: a.keyword.clone(),
        };
        let page = match self.client.contacts(&q).await {
            Ok(p) => p,
            Err(e) => return sdk_outcome(e),
        };
        let values: Vec<Value> = page
            .contacts
            .iter()
            .map(|c| {
                json!({
                    "username": c.username,
                    "displayName": c.display_name,
                    "remark": c.remark,
                    "nickname": c.nickname,
                    "alias": c.alias,
                })
            })
            .collect();
        let proj = project_messages(values, limit, CHAR_BUDGET);
        Ok(CallToolResult::structured(json!({
            "count": proj.items.len(),
            "truncated": proj.truncated,
            // 同 search_messages：截断时 hasMore 必须为真，否则调用方会当作取完了。
            "hasMore": page.has_more || proj.truncated,
            // 同 search_messages：给调用方一个**能回传**的游标（入参里有 `offset`）。
            "nextOffset": start + proj.items.len() as u64,
            "contacts": proj.items,
        })))
    }

    #[tool(description = "按句柄给出媒体访问地址。刻意不下发字节：字节会瞬间塞满模型上下文。输出会离开本机。", annotations(read_only_hint = true))]
    async fn get_media(
        &self,
        Parameters(a): Parameters<GetMediaArgs>,
    ) -> Result<CallToolResult, McpError> {
        let url = format!("{}/api/v1/media/{}", self.base_url.trim_end_matches('/'), a.handle);
        Ok(CallToolResult::structured(json!({
            "handle": a.handle,
            "url": url,
            "note": "字节经 HTTP 以 Bearer token 提供；MCP 进程刻意不把文件字节交给模型。",
        })))
    }

    #[tool(description = "群成员（名册与发言人的并集，潜水成员也会出现）。输出会离开本机。", annotations(read_only_hint = true))]
    async fn group_members(
        &self,
        Parameters(a): Parameters<GroupMembersArgs>,
    ) -> Result<CallToolResult, McpError> {
        let counts = a.include_message_counts.unwrap_or(false);
        let page = match self.client.group_members(&a.chatroom, counts).await {
            Ok(p) => p,
            Err(e) => return sdk_outcome(e),
        };
        let values: Vec<Value> = page
            .members
            .iter()
            .map(|m| {
                json!({
                    "wxid": m.wxid,
                    "displayName": m.display_name,
                    "groupNickname": m.group_nickname,
                    "remark": m.remark,
                    "alias": m.alias,
                    "isOwner": m.is_owner,
                    "isFriend": m.is_friend,
                    "messageCount": m.message_count,
                })
            })
            .collect();
        let proj = project_messages(values, MAX_LIMIT, CHAR_BUDGET);
        Ok(CallToolResult::structured(json!({
            "chatroomId": page.chatroom_id,
            "count": proj.items.len(),
            "total": page.count,
            "truncated": proj.truncated,
            "members": proj.items,
        })))
    }

    #[tool(description = "让服务端立刻跑一次增量同步。这是本面唯一的写动作：它会推进水位并可能导出媒体。输出会离开本机。")]
    async fn sync_now(&self) -> Result<CallToolResult, McpError> {
        let r = match self.client.sync_now().await {
            Ok(r) => r,
            Err(e) => return sdk_outcome(e),
        };
        Ok(CallToolResult::structured(json!({
            "success": r.success,
            "newMessages": r.new_messages,
            "revokeMessages": r.revoke_messages,
        })))
    }
}

#[tool_handler]
impl ServerHandler for WeflowMcp {
    /// 手写 get_info：instructions 里必须带上数据离机提示（模型看不到 README），
    /// 能力声明与工具集保持一处。
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "weflow-server",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(format!(
            "本地微信消息库的只读查询工具。查询全部只读，唯一的写动作是 sync_now。{OFF_MACHINE}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_accepts_relative_dates_and_unix_seconds() {
        let now = 1_700_000_000i64;
        assert_eq!(parse_since("7d", now).unwrap(), now - 7 * 86_400);
        assert_eq!(parse_since("24h", now).unwrap(), now - 24 * 3_600);
        assert_eq!(parse_since("1700000000", now).unwrap(), 1_700_000_000);
        let ymd = chrono::NaiveDate::from_ymd_opt(2023, 11, 14)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(parse_since("20231114", now).unwrap(), ymd);
        assert!(parse_since("2025-01-01", now).is_err());
        assert!(parse_since("7x", now).is_err());
        // sinceResolved 冻结的就是这个值：同一相对串随 now 前移会得到不同的
        // 绝对下界 —— 响应必须回给「本轮的」下界，续拉才不挪窗。
        let later = now + 86_400;
        assert_ne!(
            parse_since("7d", now).unwrap(),
            parse_since("7d", later).unwrap(),
            "同一相对串在不同 now 下必须解析出不同下界（这正是要回传 sinceResolved 的原因）",
        );
    }

    #[test]
    fn project_keeps_the_first_item_and_flags_truncation() {
        let items: Vec<Value> =
            (0..5).map(|i| json!({"c": "x".repeat(100), "i": i})).collect();
        let p = project_messages(items.clone(), 5, 150);
        assert_eq!(p.items.len(), 1, "一条长消息仍然要返回，不能返回空");
        assert!(p.truncated);
        let p2 = project_messages(items, 5, 10_000);
        assert_eq!(p2.items.len(), 5);
        assert!(!p2.truncated);
    }

    #[test]
    fn project_respects_the_item_cap() {
        let items: Vec<Value> = (0..5).map(|i| json!({"i": i})).collect();
        let p = project_messages(items, 2, 10_000);
        assert_eq!(p.items.len(), 2);
        assert!(p.truncated);
    }

    #[test]
    fn pick_omits_missing_and_null_keys() {
        let v = json!({"a": 1, "b": null, "c": "x"});
        assert_eq!(pick(&v, &["a", "b", "c", "d"]), json!({"a": 1, "c": "x"}));
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let svc = WeflowMcp::new("http://127.0.0.1:5033".to_string(), "super-secret".to_string());
        let s = format!("{svc:?}");
        assert!(!s.contains("super-secret"), "token 不得出现在 Debug 里: {s}");
        assert!(s.contains("redacted"), "应当显式标出被隐去的字段: {s}");
    }

    #[test]
    fn off_machine_notice_names_the_exposure() {
        assert!(OFF_MACHINE.contains("离开本机"));
    }

    #[test]
    fn limit_is_clamped_to_the_documented_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1_000)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(10)), 10);
    }
}
