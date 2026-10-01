//! **ChatLab 形状的唯一序列化出处。**
//!
//! ChatLab 面有三个消费者：Pull 面、消息面、批量导出。它们**必须给出同一个形状** —— 否则
//! 「同一份数据、两个面、字段不一样」会在下游变成一堆按来源分叉的解析代码，而那种分叉只有在
//! 某一面改字段时才会暴露。
//!
//! 这个模块负责**字段怎么填**；`server/dto.rs` 负责**字段叫什么**。分开是因为后者能给 OpenAPI 用
//! （schema 由类型生成），而前者要读 store。
//!
//! 三个面在 `replyToMessageId` 上**同规**：无引用时**省略该键**（规范把它列为可选 string，
//! 给 `null` 会让按类型读取的读者拿到解析不了的值）。这条曾经是面与面之间的差异，现在是共同契约。

use crate::server::dto::{ChatlabHeader, ChatlabMember, ChatlabMeta, MediaBrief};
use crate::store::{MessageRecord, Store};

/// 生成器标识。各面共用同一个值：下游按它区分「谁产出的」。
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
/// 群主，消息面用查询参数里解析出的群号与当前账号），但**填进哪里、怎么判类型**是同一件事。
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

/// 一条消息在 ChatLab 形状里的字段。
pub(crate) struct MessageFields {
    pub account_name: String,
    pub content: String,
    pub group_nickname: String,
    pub media: Option<MediaBrief>,
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
        media: media_brief(m),
        platform_message_id: m.server_id.to_string(),
        reply_to_message_id: m.parsed.reply_to.clone(),
        sender: m.sender_username.clone(),
        timestamp: m.create_time,
        r#type: crate::server::handlers::chatlab_type(m.local_type, &m.parsed),
    }
}

/// 一条消息的媒体元数据（拉取面与消息面同形），无媒体时 `None`（调用方据此省略整个键）。
///
/// 它是**元数据**，不是「字节可取」的承诺：只有导出确实写出了**摘要派生**的本地文件时，
/// `fileName` 才是可取句柄 —— 那一步由调用方在导出批次之后回填（见 `messages::run_export_batch`）。
pub(crate) fn media_brief(m: &MessageRecord) -> Option<MediaBrief> {
    m.parsed.media.as_ref().map(|hint| MediaBrief {
        file_name: hint.file_name.clone(),
        md5: hint.md5.clone(),
        r#type: hint.kind.as_str().to_string(),
    })
}

/// 成员的字段（**各面共用**）。
///
/// `accountName` 取**联系人档案**、回退到 id：用它而不是消息里的发送者昵称，是因为昵称是
/// **消息当时的快照**（对方改名后不会变），而成员表描述的是「这个人现在叫什么」。回退到 id
/// 而不是空串 —— 空串会让下游显示一片空白，而 id 至少能定位到人（回归：各面成员表的
/// `accountName` 非空断言）。
pub(crate) fn member_fields(store: &Store, chatroom: Option<&str>, username: &str) -> ChatlabMember {
    let c = store.contacts.get(username);
    let profile = c
        .map(|c| c.display_name())
        .filter(|s| !s.is_empty() && s != username);
    ChatlabMember {
        account_name: profile.unwrap_or_else(|| username.to_string()),
        avatar: c.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
        group_nickname: store.group_card(chatroom, username),
        platform_id: username.to_string(),
    }
}
