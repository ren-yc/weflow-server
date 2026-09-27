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

// ── 账号面 ────────────────────────────────────────────────

/// `GET /api/v1/accounts`。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountsList {
    pub accounts: Vec<crate::server::AccountStateView>,
    pub success: bool,
}

/// 注册**被别的账号占位**（互锁）。
///
/// `occupied_by` / `occupied_status` 让客户端能记下「实际在跟哪个账号说话」，
/// 而不是无限重试。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountConflict {
    pub occupied_by: String,
    pub occupied_status: crate::server::AccountStatus,
    pub state: String,
    pub success: bool,
    pub wxid: String,
}

/// 注册**受理**（或幂等命中：`already_ready` / `in_progress`）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountRegistered {
    pub db_storage: String,
    pub state: String,
    pub status: crate::server::AccountStatus,
    pub success: bool,
    pub wxid: String,
}

/// 注销：**已成功解绑**。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountDeregistered {
    pub index_cleared: bool,
    /// 请求落地时账号处于什么状态 —— 让客户端能区分「我取消了正在进行的构建」与
    /// 「我解绑了一个就绪账号」。
    pub previous_status: crate::server::AccountStatus,
    pub purged_dirs: usize,
    pub purged_media: bool,
    pub state: String,
    pub success: bool,
    pub wxid: String,
}

/// 注销：**本来就没有绑定**。刻意幂等 —— 重试已完成的注销得到 200 而不是错误。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountNotRegistered {
    pub index_cleared: bool,
    pub purged_dirs: usize,
    pub purged_media: bool,
    pub state: String,
    pub success: bool,
    pub wxid: String,
}

/// 注销：**互锁触发**（另一个账号持有绑定，它被完全不动地留下）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AccountWxidMismatch {
    pub index_cleared: bool,
    pub occupied_by: String,
    pub occupied_status: crate::server::AccountStatus,
    pub purged_dirs: usize,
    pub purged_media: bool,
    pub state: String,
    pub success: bool,
    pub wxid: String,
}
// ── 会话列表 ──────────────────────────────────────────────

/// `GET /api/v1/sessions`（原生面）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionsNative {
    pub count: usize,
    pub sessions: Vec<SessionNative>,
    pub success: bool,
}

/// 原生面的会话项。`type` 是**平台数值枚举**（weflow：0 私聊 / 1 群 / 2 公众号 / 3 其他；
/// qqflow 的取值含义不同），`sessionType` 是同一枚举的字符串形式 —— 下游应当用后者。
#[derive(Debug, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionsChatlab {
    pub count: usize,
    pub page: Page,
    pub sessions: Vec<SessionChatlab>,
}

/// 翻页信息。**`nextCursor` 始终出现**（排空时为 `null`），不要给它加
/// `skip_serializing_if`：客户端按「键在不在」判断「还有没有下一页」会读错。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

// ── 消息（ChatLab 混合面）─────────────────────────────────

/// `GET|POST /api/v1/messages?chatlab=1`（或 `format=chatlab`）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagesChatlab {
    pub chatlab: ChatlabHeader,
    pub count: usize,
    pub has_more: bool,
    pub members: Vec<ChatlabMember>,
    pub messages: Vec<ChatlabMessage>,
    pub meta: ChatlabMeta,
    pub success: bool,
    pub talker: String,
}

/// ChatLab 信封头。`exportedAt` 是**墙钟**（每次请求都不同）——快照里靠时钟哨兵掩码。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatlabHeader {
    pub exported_at: i64,
    pub generator: String,
    pub version: String,
}

/// 会话元信息。`type` 是字符串；`ownerId` 未绑定时是空串（现状如此）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatlabMeta {
    pub group_id: String,
    pub name: String,
    pub owner_id: String,
    pub platform: String,
    pub r#type: String,
}

/// 本页出现过的发送者（去重）。`avatar` 无来源时是**空串**。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatlabMember {
    pub account_name: String,
    pub avatar: String,
    pub group_nickname: String,
    pub platform_id: String,
}

/// ChatLab 面的消息项。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatlabMessage {
    pub account_name: String,
    pub content: String,
    pub group_nickname: String,
    pub platform_message_id: String,
    /// **本面是 `null`**（键始终出现）；**Pull 面是省略该键**。两个面的差异是
    /// 有意的：本面的形状已被下游依赖，改它会破坏调用方。
    pub reply_to_message_id: Option<String>,
    pub sender: String,
    pub timestamp: i64,
    pub r#type: i64,
}
// ── SSE 事件 ──────────────────────────────────────────────

/// `message.new` 事件载荷。
///
/// **`media` 是第三种媒体形状**：只有 `type` / `fileName` / `md5` —— 推送里不含任何路径
/// 与取字节用的键（字节走 REST 的 `media=1` 导出），也**永远不含**解密密钥。推一个取不到
/// 的地址只会让客户端误以为有东西可拿。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventNew {
    pub content: String,
    /// 事件名。帧头（`event:` 行）与载荷里各有一份，**不是**重复：客户端只解析
    /// `data:` 行时也要能分辨类型。
    pub event: String,
    pub group_name: Option<String>,
    /// 无媒体时为 `null`（键保留）。
    pub media: Option<EventMedia>,
    pub rawid: String,
    pub session_id: String,
    pub session_type: String,
    pub source_name: String,
    pub timestamp: i64,
}

/// 撤销事件。形状与 `message.new` 相同但**没有 `media`** —— 撤销的是消息，不是媒体。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventRevoke {
    pub content: String,
    pub event: String,
    pub group_name: Option<String>,
    pub rawid: String,
    pub session_id: String,
    pub session_type: String,
    pub source_name: String,
    pub timestamp: i64,
}

/// 推送里的媒体元数据（**第三种形状**：无 `url` / `localPath` / `exported`）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventMedia {
    /// 建议文件名（含扩展名）。
    pub file_name: String,
    /// 原文件的 md5；取不到时为 `null`（键保留）。
    pub md5: Option<String>,
    /// 媒体大类。
    pub r#type: String,
    /// **可直接取字节的 id** —— 单段路由 `GET /api/v1/media/{id}`。
    ///
    /// **只在导出根下确有这个文件时才出现**（键随之消失，不是给 `null`）：承诺是「出现即可
    /// 取」，不是尽力而为。通告一个取不到的 id，调用方会拿到 404 并以为是服务坏了；反过来
    /// （能取到却没通告）只是少一个便捷入口，仍可走三段式路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<String>,
}

/// 水位基线/重基事件。客户端据此得知「从哪里继续拉」。
///
/// **什么时候发**：连接建立（基线）、订阅端落后（重基线）、索引重建、注销（水位归零）。
///
/// `generation` 在**注销**时递增：带着旧 `Last-Event-ID` 重连的客户端据此区分「注销后新账号
/// 刚开始」（该丢弃本地状态重新拉）与「自己漏收了」（该补拉）。少了它这两种情况在协议上是同一
/// 件事。它与新的通知面发的是**同一个**计数器。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EventSync {
    /// 事件名（`sync`）。
    pub event: String,
    /// 事件基线代号，注销时递增。
    pub generation: u64,
    /// 各表的水位线 —— 客户端从这里开始增量拉。
    pub watermarks: Vec<WatermarkEntry>,
}

/// 一张表的水位。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WatermarkEntry {
    pub table: String,
    pub watermark: WatermarkValue,
}

/// 水位三元组。三个都要：`local_id` 单独不够（同一秒可能有多条），`create_time` 单独
/// 也不够（同秒内要靠 `sort_seq` 与 `local_id` 定序）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WatermarkValue {
    pub create_time: i64,
    pub local_id: i64,
    pub sort_seq: i64,
}
// ── 消息（原生面）─────────────────────────────────────────

/// `GET|POST /api/v1/messages`（原生面）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagesNative {
    pub count: usize,
    pub has_more: bool,
    pub media: MediaEnvelope,
    pub messages: Vec<MessageNative>,
    pub success: bool,
    pub talker: String,
}

/// 本页的导出状态。`exportPath` 是绝对路径（客户端用它找导出的文件），
/// `count` 是**成功导出**的条数 —— 不是本页消息数。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaEnvelope {
    pub count: usize,
    pub enabled: bool,
    pub export_path: String,
}

/// 原生面的消息项。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageNative {
    /// `localType` 的高 32 位（appmsg 子类型）；无子类型时为 `null`。
    pub appmsg_subtype: Option<i64>,
    /// `localType` 的低位（基础类型）。两个半边分开给，免得每个消费方自己去拆
    /// `(子类型 << 32) | 基础类型` 这种打包常量。
    pub base_type: i64,
    pub content: String,
    pub create_time: i64,
    pub is_send: i64,
    pub local_id: i64,
    /// 平台原始打包值 —— 下游已依赖它，保留。
    pub local_type: i64,
    /// **始终出现**（无媒体时为 `null`），不要加 `skip_serializing_if`。
    pub media: Option<MediaObject>,
    pub parsed_content: String,
    /// **始终出现**（无引用时为 `null`）。
    pub quote: Option<Quote>,
    pub raw_content: String,
    /// **注意与 Pull 面的差异**：这里无引用时是 `null`，Pull 面是**省略该键**。
    /// 这是有意保留的既有契约（下游已依赖本面的形状），不要「统一」。
    pub reply_to_message_id: Option<String>,
    pub sender_name: String,
    pub sender_username: String,
    pub server_id: String,
    pub sort_seq: i64,
}

/// 消息的媒体元数据 —— **三种形状共用这一个 struct**。
///
/// - 未导出：`url` / `localPath` 是**空串**（不是省略），`exported` **不出现**；
/// - 导出成功：`url` 是相对路径、`localPath` 是绝对路径，且**多出** `exported: true`；
/// - SSE 版只有 `type` / `fileName` / `md5`（那两个空串字段在那边不序列化）。
///
/// `exported` 是**条件键**：不导出时它**不出现**而不是 `false`。客户端靠「键在不在」
/// 判断字节可不可取 —— 改成恒出现会让这个判据失效。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaObject {
    /// 字母序使然：`exported` 排在 `fileName` 之前，这样输出与 `json!` 的现状一致。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported: Option<bool>,
    /// 文件名**始终是字符串**；`md5` 解析不出时是 **`null`**（现状如此，勿统一成空串）。
    pub file_name: String,
    pub local_path: String,
    pub md5: Option<String>,
    pub r#type: String,
    pub url: String,
}

/// 引用（回复）的渲染信息。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub account_name: String,
    pub content: String,
    pub platform_message_id: String,
    pub sender: String,
    pub r#type: i64,
}
// ── Pull 面（/api/v1/sessions/{id}/messages）─────────────

/// ChatLab Pull 信封：**顶层就是那五块**，没有 `success` / `count`。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PullEnvelope {
    pub chatlab: ChatlabHeader,
    pub members: Vec<ChatlabMember>,
    pub messages: Vec<PullMessage>,
    pub meta: ChatlabMeta,
    pub sync: PullSync,
}

/// Pull 面的消息项。
///
/// **与混合面（`ChatlabMessage`）不是同一个 struct**：本面的 `replyToMessageId` 在无引用时
/// **省略该键**，而混合面输出 `null`。这个差异是有意的（混合面的形状已被下游依赖），
/// 所以两处必须各建 struct —— 复用会把它们的契约绑在一起。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PullMessage {
    pub account_name: String,
    pub content: String,
    pub group_nickname: String,
    pub platform_message_id: String,
    /// **省略**（不是 `null`）：规范把它列为可选 *string*，`null` 会让信任类型的读者
    /// 拿到解析不了的值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    pub sender: String,
    pub timestamp: i64,
    pub r#type: i64,
}

/// 翻页与水位。**两个游标都要原样回传**：`nextSince` 是排他下界、`nextOffset` 只用于
/// 时间戳没能前进的退化情形，客户端自行推导会跳行。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PullSync {
    pub has_more: bool,
    pub next_offset: usize,
    pub next_since: i64,
    pub watermark: i64,
}
// ── 联系人 ────────────────────────────────────────────────

/// `GET|POST /api/v1/contacts`。
#[derive(Debug, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
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
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Health {
    /// 账号阶段枚举（**不是字符串**：它的取值集合是封闭的，用类型表达比用字符串稳）。
    /// 刻意**没有**「已配置但缺密钥」这一档 —— 否则未鉴权方就能数出本机配了几个账号。
    pub account: crate::server::AccountPhase,
    pub status: String,
    pub version: String,
}

// ── 手工增量同步 ──────────────────────────────────────────

/// `POST /api/v1/sync`。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub new_messages: usize,
    pub revoke_messages: usize,
    pub success: bool,
}

/// ChatLab 面的会话项。`type` 是字符串（`group` / `private`）。
///
/// `type` 只有两个取值（规范的枚举就这么大）—— 公众号与「其它」都归到 `private`：它们都是**一对
/// 一的对话**，而规范没有第三个格子可放。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionChatlab {
    /// 会话在数据源里的唯一标识，可直接用作拉取路径。
    pub id: String,
    /// 最新消息时间戳（秒）。
    pub last_message_at: i64,
    /// 消息总数（用于 ChatLab 展示预估量）。
    pub message_count: usize,
    /// 会话名称（群名/联系人名）。
    pub name: String,
    /// 平台标识。
    pub platform: String,
    /// `group` / `private`。
    pub r#type: String,
    /// 成员数 —— **可选**：群名册的加载器拿得到才给（私聊、或名册缺失时**不出现这个键**）。
    ///
    /// 规范把它列为可选，而「没有名册」与「名册是空的」在下游是两件事：前者不该被读成 0。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<usize>,
}

// ── ChatLab 通知帧 ────────────────────────────────────────

/// `/chatlab/push/messages` 的通知帧：**只带元信息，不带正文**。
///
/// 规范对这条通道的定位是「仅通知：不假设事件可靠送达」—— 客户端收到后**去拉**那一页。
/// 带正文会诱导调用方把它当数据源，而它并不保证送达；不带，语义就没有歧义。
///
/// `eventId` 与 `platformMessageId` 是**两个不同的号**：前者是事件通道自己的标识，后者是那条
/// 消息在平台上的 id（拉取时用它定位）。撤回事件里 `platformMessageId` 是被撤回那条的 id。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationFrame {
    /// 事件名。帧头（`event:` 行）与载荷里各有一份，**不是**重复：只解析 `data:` 行的客户端
    /// 也要能分辨类型。
    pub event: String,
    /// 事件通道自己的标识。
    pub event_id: String,
    /// 平台消息 id；取不到时为 `null`（键保留）。
    pub platform_message_id: Option<String>,
    /// 所属会话。
    pub session_id: String,
    /// 事件时刻（秒）。
    pub timestamp: i64,
}

/// `/chatlab/push/messages` 的基线帧。
///
/// `generation` 在注销时递增（见 `server::GENERATION`）：带着旧 `Last-Event-ID` 重连的客户端
/// 据此区分「注销后新账号刚开始」与「自己漏收了」—— 前者该丢弃本地状态重新拉，后者该补拉。
/// 少了它，这两种情况在协议上是同一件事。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncFrame {
    /// 事件名（`sync`）。
    pub event: String,
    /// 基线代号。
    pub generation: u64,
    /// 各表的水位线 —— 客户端从这里开始增量拉。
    pub watermarks: Vec<WatermarkEntry>,
}
