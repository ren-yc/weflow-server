//! In-memory index over live read-only connections (mirrors qqflow-server's store
//! skeleton, WeChat-ized). Everything the HTTP API and SSE push serve comes
//! from this structure; queries never touch the encrypted databases.

pub mod index;

use std::collections::HashMap;

use crate::parser::ParsedMsg;

/// Session/contact kind classification by username conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum SessionKind {
    #[default]
    Private,
    Group,
    Official,
    Other,
}

impl SessionKind {
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


#[derive(Debug, Clone)]
pub struct Session {
    pub username: String,
    pub display_name: String,
    pub kind: SessionKind,
    pub last_timestamp: i64,
    pub last_msg_type: Option<i64>,
    pub summary: Option<String>,
    pub unread_count: i64,
    /// Message count (filled from the conv index, may be an estimate).
    pub message_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Contact {
    pub username: String,
    pub remark: Option<String>,
    pub nickname: Option<String>,
    pub alias: Option<String>,
    pub avatar_url: Option<String>,
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

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub local_id: i64,
    pub server_id: i64,
    pub local_type: i64,
    pub create_time: i64,
    pub sort_seq: i64,
    pub is_send: bool,
    pub sender_username: String,
    /// Display name resolved through contacts/Name2Id at index time.
    pub sender_name: String,
    pub parsed: ParsedMsg,
}

/// Incremental watermark per message table (`<rel>:<table>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Watermark {
    pub create_time: i64,
    pub sort_seq: i64,
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
    pub create_time: i64,
    /// Text content (`<contentDesc>`), empty for pure-media posts
    pub content_desc: String,
    /// Coarse kind: text | image | video
    pub kind: &'static str,
    /// Numeric `ContentObject.type` as string (WeFlow `type`)
    pub content_type: String,
    pub media: Vec<SnsMedia>,
    pub comment_count: usize,
    /// Likes (`like_user_list` blocks)
    pub likes: Vec<SnsPerson>,
    /// Comments (user_comment blocks carrying content)
    pub comments: Vec<SnsComment>,
    pub latitude: f64,
    pub longitude: f64,
    /// Full original XML (WeFlow returns `rawXml`)
    pub raw_xml: String,
}

/// A liker on a moment.
#[derive(Debug, Clone, Default)]
pub struct SnsPerson {
    pub username: String,
    pub nickname: String,
    pub create_time: i64,
}

/// A comment on a moment.
#[derive(Debug, Clone, Default)]
pub struct SnsComment {
    pub username: String,
    pub nickname: String,
    pub create_time: i64,
    pub content: String,
}

/// One media item inside a moment (`<mediaList><media>`).
#[derive(Debug, Clone, Default)]
pub struct SnsMedia {
    /// image | video
    pub kind: &'static str,
    pub md5: Option<String>,
    /// Full-size CDN url (`<url>` element text)
    pub url: String,
    /// Thumbnail CDN url (`<thumb>` element text)
    pub thumb: Option<String>,
    pub width: i64,
    pub height: i64,
    /// CDN access material (passthrough for proxy clients)
    pub token: Option<String>,
    pub key: Option<String>,
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
    /// chatroom username -> sender username -> display name (group cards)
    pub group_cards: GroupCards,
    /// `<rel>:<table>` -> watermark of the last indexed row
    pub watermarks: HashMap<String, Watermark>,
    /// Moments timeline, sorted newest-first
    pub sns_feeds: Vec<SnsFeed>,
}

impl Store {
    pub fn is_empty(&self) -> bool {
        self.convs.is_empty() && self.sessions.is_empty()
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