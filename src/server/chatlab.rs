//! **ChatLab 形状的唯一序列化出处。**
//!
//! ChatLab 面有四个消费者：REST 的 `format=chatlab`、Pull 面、批量导出、MCP 输出。它们**必须给出
//! 同一个形状** —— 否则「同一份数据、两个面、字段不一样」会在下游变成一堆按来源分叉的解析代码，
//! 而那种分叉只有在某一面改字段时才会暴露。
//!
//! 这个模块负责**字段怎么填**；`server/dto.rs` 负责**字段叫什么**。分开是因为后者能给 OpenAPI 用
//! （schema 由类型生成），而前者要读 store。
//!
//! **有意保留的差异只有一处**：`replyToMessageId`。规范把它列为可选 string，Pull 面因此在该键缺席
//! 时**不输出它**（`skip_serializing_if`），而混合面按既有契约输出 `null`（下游已依赖）。两处由此
//! 各建 struct —— 这是**契约差异**，不是漏抽。

use crate::server::dto::{ChatlabHeader, ChatlabMeta};
use crate::store::{MessageRecord, Store};

/// 生成器标识。两个面共用同一个值：下游按它区分「谁产出的」。
pub(crate) const GENERATOR: &str = "weflow-server";

/// ChatLab 格式版本。
///
/// 它是**格式**的版本，不是本服务的版本 —— 升级本服务不会让它变。
pub(crate) const FORMAT_VERSION: &str = "0.0.2";

/// 平台标识。
pub(crate) const PLATFORM: &str = "wechat";

pub(crate) fn header() -> ChatlabHeader {
    ChatlabHeader {
        exported_at: chrono::Utc::now().timestamp(),
        generator: GENERATOR.to_string(),
        version: FORMAT_VERSION.to_string(),
    }
}

/// 会话元数据。
///
/// `group_id` 与 `owner_id` 由调用方给：两个面拿它们的来源不同（Pull 面用路径参数与会话自身的
/// 群主，混合面用查询参数里解析出的群号与当前账号），但**填进哪里、怎么判类型**是同一件事。
pub(crate) fn meta(
    store: &Store,
    talker: &str,
    group_id: String,
    owner_id: String,
) -> ChatlabMeta {
    ChatlabMeta {
        group_id,
        name: store.session_display(talker),
        owner_id,
        platform: PLATFORM.to_string(),
        r#type: session_type(talker).to_string(),
    }
}

/// 会话类型：群聊还是私聊。
///
/// 判据是 talker 的后缀（`@chatroom`），**不是**会话在索引里的类型 —— 两者在正常数据上一致，
/// 而这里要的是 ChatLab 规范的 `group` / `private`，它对「公众号」「其它」没有格子，都归 private。
/// 两处各写一遍时，改一处忘另一处就会让同一个会话在两个面上类型不同。
pub(crate) fn session_type(talker: &str) -> &'static str {
    if talker.ends_with("@chatroom") { "group" } else { "private" }
}

/// 一条消息在 ChatLab 形状里的字段 —— 除 `replyToMessageId` 之外的全部。
///
/// 单独成一个 struct 而不是直接给 `ChatlabMessage`：那两个 struct 的 `replyToMessageId` 序列化
/// 行为**按契约不同**（见模块头），把公共部分抽到这里、差异留给各自的 struct。
pub(crate) struct MessageFields {
    pub account_name: String,
    pub content: String,
    pub group_nickname: String,
    pub platform_message_id: String,
    pub reply_to_message_id: Option<String>,
    pub sender: String,
    pub timestamp: i64,
    pub r#type: i64,
}

/// 把一条记录映射成 ChatLab 的字段。
///
/// `chatroom` 是群号；私聊传 `None`（群名片在私聊里没有意义，`group_card` 会回空串）。
pub(crate) fn message_fields(
    store: &Store,
    chatroom: Option<&str>,
    m: &MessageRecord,
) -> MessageFields {
    MessageFields {
        account_name: m.sender_name.clone(),
        content: m.parsed.display.clone(),
        group_nickname: store.group_card(chatroom, &m.sender_username),
        platform_message_id: m.server_id.to_string(),
        reply_to_message_id: m.parsed.reply_to.clone(),
        sender: m.sender_username.clone(),
        timestamp: m.create_time,
        r#type: crate::server::handlers::chatlab_type(m.local_type, &m.parsed),
    }
}

// ── 成员列表：**两个面目前不一致，因此这里不抽** ──────────────────
//
// 实测差异只在 `accountName` 一个字段上：
//
//   Pull 面：消息里推出来的 `senderName`
//   混合面：联系人档案的 `display_name()`，**回退到 id**
//
// 其余三个字段（`avatar` / `groupNickname` / `platformId`）两处一致。
//
// **为什么先不动**：它改的是**已有面的可见输出**（混合面有下游在解析），而不是内部结构。按迁移
// 政策的「新增能力走新面、缺陷修复改面」，这属于后者 —— 得先决定**哪一个才对**，而不是顺手统一。
//
// **倾向**：以联系人档案为准（它是权威来源，且混合面已经在这么做），把 Pull 面的回退从「消息昵称」
// 改成「id」与之一致。但那要作为一项**登记过的行为变更**走，不是这次重构的副产品。