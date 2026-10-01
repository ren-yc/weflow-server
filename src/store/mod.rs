//! In-memory index over live read-only connections (mirrors qqflow-server's store
//! skeleton, WeChat-ized). Everything the HTTP API and SSE push serve comes
//! from this structure; queries never touch the encrypted databases.

pub mod group_meta;
pub mod index;

use std::collections::HashMap;

use crate::parser::ParsedMsg;

/// Session/contact kind classification by username conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
/// 会话类型。
///
/// 由 `username` 的形状推出，**不需要额外数据**：`@chatroom` 结尾是群，`gh_` 开头是公众号。
/// 因此它对任何 username 都有答案（推不出时是 [`SessionKind::Private`]），调用方不必处理
/// 「未知类型」这一支。
pub enum SessionKind {
    /// 私聊（也是推不出其它类型时的默认值）。
    #[default]
    Private,
    /// 群聊：`username` 以 `@chatroom` 结尾。
    Group,
    /// 公众号：`username` 以 `gh_` 开头。
    Official,
    /// 预留 —— 当前 [`SessionKind::classify`] 不会产生它。
    Other,
}

impl SessionKind {
    /// 稳定的字符串形式（`private` / `group` / `official` / `other`），可直接进日志或 JSON。
    ///
    /// 这几个值是对外契约的一部分 —— 改动它们等同于破坏性变更。
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Private => "private",
            SessionKind::Group => "group",
            SessionKind::Official => "official",
            SessionKind::Other => "other",
        }
    }

    /// WeChat conventions: `@chatroom` suffix = group, `gh_` = official.
    pub fn classify(username: &str) -> SessionKind {
        if username.ends_with("@chatroom") {
            SessionKind::Group
        } else if username.starts_with("gh_") {
            SessionKind::Official
        } else {
            SessionKind::Private
        }
    }
}


/// 一个会话（聊天）的摘要。
#[derive(Debug, Clone)]
pub struct Session {
    /// 会话的稳定 id（私聊是对方 wxid，群是 `…@chatroom`）。
    pub username: String,
    /// 展示名（群名或联系人名）；取不到时为空串。
    pub display_name: String,
    /// 会话类型。
    pub kind: SessionKind,
    /// 最后一条消息的时刻（秒）。
    pub last_timestamp: i64,
    /// 最后一条消息的类型码；没有消息时为 `None`（`0` 是合法类型码，不能当哨兵）。
    pub last_msg_type: Option<i64>,
    /// 会话列表里显示的那行预览；没有时为 `None`。
    pub summary: Option<String>,
    /// 未读数。
    pub unread_count: i64,
    /// Message count (filled from the conv index, may be an estimate).
    pub message_count: usize,
}

/// 一个联系人。
///
/// **`remark` 与群名片是两回事。** 前者是「你给他起的备注」，后者是「他在某个群里的名片」
/// （见 [`crate::api::Index::group_card`]）。实测有成员在不同群里名片各不相同，把备注当名片
/// 会显示错人。
#[derive(Debug, Clone, Default)]
pub struct Contact {
    /// wxid。
    pub username: String,
    /// 你给他起的备注；没设为 `None`。
    pub remark: Option<String>,
    /// 他自己的昵称；没有为 `None`。
    pub nickname: Option<String>,
    /// 微信号（alias）；没设为 `None`。
    pub alias: Option<String>,
    /// 头像地址；没有为 `None`。
    pub avatar_url: Option<String>,
    /// 这个联系人对应的会话类型。
    pub kind: SessionKind,
}

impl Contact {
    /// Display priority: remark > nickname > username (WeFlow rule).
    pub fn display_name(&self) -> String {
        self.remark
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.nickname.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or(&self.username)
            .to_string()
    }
}

/// 一条消息。
#[derive(Debug, Clone)]
pub struct MessageRecord {
    /// 库内自增行号，同一 `(create_time, sort_seq)` 下的最终定序依据。
    pub local_id: i64,
    /// 服务端 id（微信分配）。**不保证唯一**：不同会话里可能出现同一个值。
    pub server_id: i64,
    /// 微信自己的类型码（与 ChatLab 的类型枚举是**两套**编号）。
    pub local_type: i64,
    /// 发送时刻（秒）。
    pub create_time: i64,
    /// 同一秒内的排序号 —— 只靠秒级时间戳无法定序。
    pub sort_seq: i64,
    /// 是否由本账号发出。
    pub is_send: bool,
    /// 发送者的 wxid。
    pub sender_username: String,
    /// 展示名：建索引时经 contacts/Name2Id 解析好的，调用方不必再查一次。
    pub sender_name: String,
    /// 解析后的内容视图（正文、媒体、引用、撤回）。原始 XML 在 `parsed.raw_content` 里。
    pub parsed: ParsedMsg,
}

/// Incremental watermark per message table (`<rel>:<table>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Watermark {
    /// 消息的 `create_time`（秒）。
    pub create_time: i64,
    /// 同一秒内的排序号 —— 只靠秒级时间戳无法定序，微信同秒会发多条。
    pub sort_seq: i64,
    /// 自增行号，同一 `(create_time, sort_seq)` 下的最终定序依据。
    pub local_id: i64,
}

/// Per-chatroom sender display names (group cards etc.).
pub type GroupCards = HashMap<String, HashMap<String, String>>;

/// One moment (朋友圈 feed) parsed from `sns.db:SnsTimeLine`.
#[derive(Debug, Clone, Default)]
pub struct SnsFeed {
    /// `SnsTimeLine.tid` (may be negative; used as the stable id)
    pub feed_id: String,
    /// Poster's username (`SnsTimeLine.user_name`)
    pub user_name: String,
    /// `TimelineObject.id` (WeFlow contract exposes both tid and id)
    pub object_id: String,
    /// Poster nickname from the payload (`LocalExtraInfo.nickname`)
    pub nickname: String,
    /// 发布时刻（秒）。
    pub create_time: i64,
    /// Text content (`<contentDesc>`), empty for pure-media posts
    pub content_desc: String,
    /// Coarse kind: text | image | video
    pub kind: &'static str,
    /// Numeric `ContentObject.type` as string (WeFlow `type`)
    pub content_type: String,
    /// 这条朋友圈里的图片/视频，顺序与 XML 里一致。
    pub media: Vec<SnsMedia>,
    /// 评论条数（与 `comments` 的长度一致；单独给出是为了不让调用方为了计数而解析数组）。
    pub comment_count: usize,
    /// Likes (`like_user_list` blocks)
    pub likes: Vec<SnsPerson>,
    /// Comments (user_comment blocks carrying content)
    pub comments: Vec<SnsComment>,
    /// 定位纬度；未携带定位时为 `0.0`（不是「赤道」——`0.0/0.0` 是不存在的几内亚湾坐标，
    /// 用作哨兵）。
    pub latitude: f64,
    /// 定位经度；语义同 `latitude`。
    pub longitude: f64,
    /// Full original XML (WeFlow returns `rawXml`)
    pub raw_xml: String,
}

/// A liker on a moment.
#[derive(Debug, Clone, Default)]
pub struct SnsPerson {
    /// 点赞者的 wxid。
    pub username: String,
    /// 点赞时的昵称（历史快照 —— 对方改名后这里不会跟着变）。
    pub nickname: String,
    /// 点赞时刻（秒）。
    pub create_time: i64,
}

/// A comment on a moment.
#[derive(Debug, Clone, Default)]
pub struct SnsComment {
    /// 评论者的 wxid。
    pub username: String,
    /// 评论时的昵称（历史快照）。
    pub nickname: String,
    /// 评论时刻（秒）。
    pub create_time: i64,
    /// 评论正文。
    pub content: String,
}

/// One media item inside a moment (`<mediaList><media>`).
#[derive(Debug, Clone, Default)]
pub struct SnsMedia {
    /// image | video
    pub kind: &'static str,
    /// 原图/原视频的 md5；取不到时为 `None`（不是空串 —— 「没有」与「是空串」在这里含义不同）。
    pub md5: Option<String>,
    /// Full-size CDN url (`<url>` element text)
    pub url: String,
    /// Thumbnail CDN url (`<thumb>` element text)
    pub thumb: Option<String>,
    /// 像素宽；未知时为 `0`。
    pub width: i64,
    /// 像素高；未知时为 `0`。
    pub height: i64,
    /// CDN access material (passthrough for proxy clients)
    pub token: Option<String>,
    /// 解码密钥材料（原样透传给代理客户端，本 crate 不解读它）。
    pub key: Option<String>,
    /// 加密索引（同上，原样透传）。
    pub enc_idx: Option<String>,
}

/// The whole in-memory index (single `parking_lot::RwLock<Store>` in the API).
#[derive(Debug, Clone, Default)]
pub struct Store {
    pub my_wxid: String,
    /// username -> session summary
    pub sessions: HashMap<String, Session>,
    /// username -> messages (append-only; sorted lazily by the query layer)
    pub convs: HashMap<String, Vec<MessageRecord>>,
    /// username -> contact profile
    pub contacts: HashMap<String, Contact>,
    /// chatroom username -> sender username -> 群名片。
    ///
    /// 来源是 `contact/contact_fts.db` 的 FTS **影子表**（见 [`crate::store::group_meta`]），
    /// 不是联系人备注 —— 两者是不同字段，且实测有 335 个成员在不同群里名片不同。
    pub group_cards: GroupCards,
    /// chatroom username -> 群主 username（`contact.db::chat_room.owner`）。
    pub chatroom_owner: HashMap<String, String>,
    /// chatroom username -> 名册（`contact.db::chatroom_member`）。
    ///
    /// 它有两个用处：ChatLab 面的 `memberCount`（真值），以及 `group-members` 的成员集合
    /// —— 后者是**名册 ∪ 发言人**：只列发言人会让潜水成员永远不出现，而「群里有谁」的答案
    /// 不该取决于谁最近说过话。
    pub chatroom_roster: HashMap<String, Vec<String>>,
    /// 索引**最近一次构建或增量更新完成**的时刻（毫秒，墙钟）。
    ///
    /// 它与成员表一起下发（`updatedAt`），让客户端判断这份数据有多旧。用墙钟而不是单调
    /// 时钟，是因为它要跨进程重启保持可比；用毫秒是因为同一秒内可能发生两次更新。
    pub index_built_at_ms: i64,
    /// `<rel>:<table>` -> watermark of the last indexed row
    pub watermarks: HashMap<String, Watermark>,
    /// Moments timeline, sorted newest-first
    pub sns_feeds: Vec<SnsFeed>,
}

impl Store {
    pub fn is_empty(&self) -> bool {
        self.convs.is_empty() && self.sessions.is_empty()
    }

    /// 记下索引刚刚构建/更新完成（见 `index_built_at_ms`）。
    pub fn mark_index_built(&mut self) {
        self.index_built_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
    }

    /// Best display name for a session: session display name, else the
    /// contact's display name, else the raw username.
    pub fn session_display(&self, username: &str) -> String {
        self.session_display_opt(username)
            .unwrap_or_else(|| username.to_string())
    }

    /// Like `session_display`, but `None` when no *name* is known (rather
    /// than echoing the username back). Callers whose empty value means
    /// "unknown" to a downstream client use this, so a raw wxid is never
    /// presented as if it were a name — real accounts do contain groups that
    /// were never named and have no contact entry to borrow a name from.
    pub fn session_display_opt(&self, username: &str) -> Option<String> {
        if let Some(s) = self.sessions.get(username)
            && !s.display_name.is_empty() {
                return Some(s.display_name.clone());
            }
        self.contacts
            .get(username)
            .map(|c| c.display_name())
            .filter(|s| !s.is_empty() && s != username)
    }

    /// Best display name for a message sender inside a chatroom context.
    pub fn sender_display(&self, chatroom: Option<&str>, sender: &str, fallback: &str) -> String {
        if let Some(room) = chatroom
            && let Some(cards) = self.group_cards.get(room)
                && let Some(card) = cards.get(sender).filter(|s| !s.is_empty()) {
                    return card.clone();
                }
        if let Some(c) = self.contacts.get(sender) {
            let d = c.display_name();
            if d != sender {
                return d;
            }
        }
        fallback.to_string()
    }

    /// The sender's per-chatroom group card (群昵称) alone, empty when they
    /// have none or the chat is not a group.
    ///
    /// This is the other half of [`Store::sender_display`], which merges the
    /// card and the contact name into one value. ChatLab keeps them apart
    /// (`accountName` vs `groupNickname`), so its faces need the card on its
    /// own — a contact's 备注 is NOT a group nickname and must not be served
    /// as one.
    pub fn group_card(&self, chatroom: Option<&str>, sender: &str) -> String {
        chatroom
            .and_then(|room| self.group_cards.get(room))
            .and_then(|cards| cards.get(sender))
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default()
    }

    /// Estimate the total message count of a conversation.
    pub fn conv_count(&self, username: &str) -> usize {
        self.convs.get(username).map_or(0, |v| v.len())
    }

    /// Total number of indexed messages across all conversations.
    pub fn total_messages(&self) -> usize {
        self.convs.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_rules() {
        assert_eq!(SessionKind::classify("wxid_abc"), SessionKind::Private);
        assert_eq!(SessionKind::classify("group@chatroom"), SessionKind::Group);
        assert_eq!(SessionKind::classify("gh_abc123"), SessionKind::Official);
    }

    #[test]
    fn contact_display_priority() {
        let c = Contact {
            username: "wxid_1".into(),
            remark: Some("老板".into()),
            nickname: Some("张三".into()),
            ..Default::default()
        };
        assert_eq!(c.display_name(), "老板");
        let c2 = Contact {
            username: "wxid_2".into(),
            nickname: Some("李四".into()),
            ..Default::default()
        };
        assert_eq!(c2.display_name(), "李四");
    }

    /// `session_display` echoes the username as a last resort (a list needs
    /// *something* to show); `session_display_opt` reports the same case as
    /// `None` so an SSE field whose empty value means "unknown" never ships a
    /// raw wxid dressed up as a group name.
    #[test]
    fn session_display_opt_separates_unknown_from_id_fallback() {
        let mut store = Store::default();
        // a session with no name column value and no contact row
        store.sessions.insert(
            "room@chatroom".into(),
            Session {
                username: "room@chatroom".into(),
                display_name: String::new(),
                kind: SessionKind::Group,
                last_timestamp: 0,
                last_msg_type: None,
                summary: None,
                unread_count: 0,
                message_count: 0,
            },
        );
        assert_eq!(store.session_display("room@chatroom"), "room@chatroom");
        assert_eq!(store.session_display_opt("room@chatroom"), None);

        // contacts supply the name -> both agree
        store.contacts.insert(
            "room@chatroom".into(),
            Contact {
                username: "room@chatroom".into(),
                nickname: Some("项目群".into()),
                kind: SessionKind::Group,
                ..Default::default()
            },
        );
        assert_eq!(store.session_display("room@chatroom"), "项目群");
        assert_eq!(store.session_display_opt("room@chatroom"), Some("项目群".into()));

        // a contact whose only "name" is the username itself is not a name
        store.contacts.insert(
            "wxid_bare".into(),
            Contact {
                username: "wxid_bare".into(),
                ..Default::default()
            },
        );
        assert_eq!(store.session_display("wxid_bare"), "wxid_bare");
        assert_eq!(store.session_display_opt("wxid_bare"), None);
    }
}
