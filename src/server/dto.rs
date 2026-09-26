//! 响应体的**类型化定义**（HTTP 形状的单一事实源）。
//!
//! 为什么要有这一层：`json!` 里的键名是字符串字面量，拼错了只有运行时才知道；
//! 「哪些键在什么条件下出现」只存在于代码路径里。DTO 把这些变成类型，编译器守住它。
//!
//! ## 三条纪律
//!
//! 1. **字段按字母序声明。** `json!` 走 `serde_json::Map`（默认 BTreeMap），因此现有响应
//!    的键**是按字母序输出的**；struct 的序列化顺序是**声明顺序**，所以按字母序声明能让
//!    DTO 的输出与现状**逐字节一致** —— 「换 DTO」不该顺带改动任何一个响应。
//!    顺序本身不破坏 JSON 语义，但会让 review 淹没在无意义的 diff 里，而快照的 `keys`
//!    字段会把它抓出来。
//! 2. **`null` 与「省略」是两件事，逐键保留现状**：要输出 `null` 就写 `Option<T>` 且
//!    **不加** `skip_serializing_if`；要省略才加。任何「顺手统一风格」都会踩契约。
//! 3. **同名键在两端点若类型或来源不同，必须各自建 struct**。`type` 就是例子：
//!    原生面是数字（平台枚举），ChatLab 面是字符串 —— 一个 struct 不可能同时满足。
//!
//! ## 与 `json!` 的关系
//!
//! 这些 struct **不改变契约**，只是把已有的形状写下来。迁移按端点逐个进行，每换一个
//! 都要跑 golden 快照 —— 它才是判断「有没有改到响应」的东西。

use serde::Serialize;

// ── 会话列表 ──────────────────────────────────────────────

/// `GET /api/v1/sessions`（原生面）。
#[derive(Debug, Serialize)]
pub struct SessionsNative {
    pub count: usize,
    pub sessions: Vec<SessionNative>,
    pub success: bool,
}

/// 原生面的会话项。`type` 是**平台数值枚举**（weflow：0 私聊 / 1 群 / 2 公众号 / 3 其他；
/// qqflow 的取值含义不同），`sessionType` 是同一枚举的字符串形式 —— 下游应当用后者。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNative {
    pub display_name: String,
    pub last_timestamp: i64,
    pub message_count: usize,
    pub session_type: String,
    /// **始终出现，无摘要时为 `null`** —— `Option` 且**不加** `skip_serializing_if`。
    /// 这种「有键但值为空」与「没有这个键」的区别是契约的一部分，不要顺手统一。
    pub summary: Option<String>,
    pub r#type: i64,
    pub unread_count: i64,
    pub username: String,
}

/// `GET /api/v1/sessions?chatlab=1`（或 `format=chatlab`）。
///
/// **与原生面的键集不同**，且 `type` 在这里是字符串 —— 两个面不可能共用一个 struct。
#[derive(Debug, Serialize)]
pub struct SessionsChatlab {
    pub count: usize,
    pub page: Page,
    pub sessions: Vec<SessionChatlab>,
}

/// 翻页信息。**`nextCursor` 始终出现**（排空时为 `null`），不要给它加
/// `skip_serializing_if`：客户端按「键在不在」判断「还有没有下一页」会读错。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

// ── 联系人 ────────────────────────────────────────────────

/// `GET|POST /api/v1/contacts`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Contacts {
    pub contacts: Vec<Contact>,
    pub count: usize,
    pub has_more: bool,
    pub success: bool,
    pub total: usize,
}

/// 联系人项。
///
/// **缺字段一律是空串，绝不是 `null`**。`store::Contact` 内部用 `Option<String>`（因为
/// `display_name()` 要区分「没有备注」与「备注是空串」），但**只有 JSON 边界**把它压平成
/// 空串 —— 这个压平是契约的一部分，已在别处钉死。改回去会静默改变每个下游的判空逻辑。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    pub alias: String,
    pub avatar_url: String,
    pub display_name: String,
    pub nickname: String,
    pub remark: String,
    pub r#type: String,
    pub username: String,
}

// ── 群成员 ────────────────────────────────────────────────

/// `GET|POST /api/v1/group-members`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMembers {
    pub chatroom_id: String,
    pub count: usize,
    pub members: Vec<GroupMember>,
    pub refreshed: bool,
    pub success: bool,
}

/// 群成员项。字段缺失同样压平成**空串**（与 `contacts` 同规）。
///
/// `isOwner` 目前恒为 `false`（群主信息不在已解析的表中）；`messageCount` 仅在
/// `includeMessageCounts=1` 时为真实值。两个「暂时恒定的字段」都要留着键 —— 删掉它们
/// 会让下游的字段存在性判断失效。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMember {
    pub alias: String,
    pub avatar_url: String,
    pub display_name: String,
    pub group_nickname: String,
    pub is_friend: bool,
    pub is_owner: bool,
    pub message_count: i64,
    pub nickname: String,
    pub remark: String,
    pub wxid: String,
}
// ── 健康检查 ──────────────────────────────────────────────

/// `GET|POST /health` 与 `/api/v1/health`（**免鉴权**）。
///
/// 刻意是标量：未鉴权方可访问，因此**不能**列出账号 —— 连数组长度都会泄露
/// 「本机有几个账号、各自到哪一步」。账号身份与消息数走鉴权的 `/api/v1/accounts`。
#[derive(Debug, Serialize)]
pub struct Health {
    /// 账号阶段枚举（**不是字符串**：它的取值集合是封闭的，用类型表达比用字符串稳）。
    /// 刻意**没有**「已配置但缺密钥」这一档 —— 否则未鉴权方就能数出本机配了几个账号。
    pub account: crate::server::AccountPhase,
    pub status: String,
    pub version: String,
}

// ── 手工增量同步 ──────────────────────────────────────────

/// `POST /api/v1/sync`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub new_messages: usize,
    pub revoke_messages: usize,
    pub success: bool,
}

/// ChatLab 面的会话项。`type` 是字符串（`group` / `private`）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChatlab {
    pub id: String,
    pub last_message_at: i64,
    pub message_count: usize,
    pub name: String,
    pub platform: String,
    pub r#type: String,
}
