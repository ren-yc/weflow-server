//! 嵌入者承诺面。
//!
//! **这是本 crate 唯一的对外承诺。** 其余模块在默认构建下是 `pub(crate)` —— 外部不可达，边界由
//! 编译器强制。承诺面有 rustdoc、有 semver 承诺，`#![deny(missing_docs)]` 也只作用在这里。
//!
//! ## 为什么需要这层间接
//!
//! [`crate::store::Store`] 的字段是 `pub`（内部模块要写它）。直接把 `Store` 放出来，等于把
//! **每一个字段**都变成对外契约 —— 它们一变动下游就断，而它们本来是内部布局。
//!
//! 所以这里只放**读**：嵌入者能查，不能改。写路径（同步引擎、索引构建）留在实现面，
//! 由本 crate 自己驱动。
//!
//! ## 与 HTTP 面的关系
//!
//! HTTP 面的响应形状由 `server::dto` 描述（typed DTO 是那一面的单一事实源）。这里**不是**它的
//! 副本：嵌入者拿到的就是索引里的类型，不经 JSON 往返。

use std::sync::Arc;

use parking_lot::RwLock;

use crate::db::live::LivePool;
use crate::store::Store;

// 密钥：嵌入者**必须**能拿到它才能建索引 —— 没有密钥就没有读，所以它是承诺面的一部分。
// 但「密钥从哪来」不在这里：本 crate 不做密钥提取，调用方自己负责（见 [`open`]）。
pub use crate::keystore::{KeyMap, parse_db_key};

// 承诺面放出去的是**数据本身** —— 它们的字段就是契约（嵌入者要读它们）。
// `Store` **不在**这一列里：它的字段是内部布局，直接放出去等于把每个字段都变成契约。
pub use crate::store::{
    Contact, GroupCards, MessageRecord, Session, SessionKind, SnsComment, SnsFeed, SnsMedia,
    SnsPerson, Watermark,
};

// `MessageRecord::parsed` 是公开字段，它的类型因此也必须可达 —— 否则等于泄漏一个私有类型。
// 同理还有 `ParsedMsg` 自己的几个字段类型。
pub use crate::parser::{MediaHint, ParsedMsg, QuoteInfo, RevokeInfo};

/// 从一份账号目录建立索引，**不起 HTTP**。
///
/// 这是嵌入者通常的第一个调用：读 `db_storage` 下所有库、解密、建内存索引。**同步**（不返回
/// future）—— 真实账号几万条消息是秒级到十几秒，调用方该自己决定放在哪个线程。
///
/// `keys` 是各库的密钥表（`db_storage` 里的相对路径 -> 密钥）；密钥从哪来由调用方负责，
/// 本 crate 不做密钥提取。
///
/// 缺库、缺密钥、某个库读不动，都只是**少索引一部分**，不会让整个调用失败 —— 「能读到多少
/// 算多少」比「一个库坏了就全不可用」更符合只读服务的使用场景。
pub fn open(
    storage: &std::path::Path,
    keys: &KeyMap,
    my_wxid: &str,
) -> anyhow::Result<Index> {
    let files = crate::db::scan::enum_db_files(storage);
    let mut pool = LivePool::new();
    let store = crate::store::index::build_all_live(&mut pool, keys, my_wxid, &files)?;
    Ok(Index::new(Arc::new(RwLock::new(store))))
}

/// 一个账号的只读索引句柄。
///
/// 克隆是廉价的（内部是 `Arc`）—— 嵌入者可以放心把它分给多个线程。
#[derive(Clone)]
pub struct Index {
    store: Arc<RwLock<Store>>,
}

impl Index {
    /// 包住一个索引。
    ///
    /// 索引的**构建**不在这里：那是写路径，由 [`crate::sync`] 与 [`crate::store::index`] 驱动。
    /// 嵌入者拿到的是已经建好的那一个。
    pub fn new(store: Arc<RwLock<Store>>) -> Self {
        Self { store }
    }

    /// 本账号的 wxid。
    pub fn wxid(&self) -> String {
        self.store.read().my_wxid.clone()
    }

    /// 索引是否还没建起来（零账号启动、或构建尚未完成时为真）。
    pub fn is_empty(&self) -> bool {
        self.store.read().is_empty()
    }

    /// 全部会话，按 `username` 升序 —— 顺序稳定，便于调用方做 diff。
    pub fn sessions(&self) -> Vec<Session> {
        let store = self.store.read();
        let mut out: Vec<Session> = store.sessions.values().cloned().collect();
        out.sort_by(|a, b| a.username.cmp(&b.username));
        out
    }

    /// 会话的展示名（无则回落到 username）。
    pub fn session_display(&self, username: &str) -> String {
        self.store.read().session_display(username)
    }

    /// 一个会话的消息，按时间升序。未知会话返回空。
    pub fn messages(&self, username: &str) -> Vec<MessageRecord> {
        let store = self.store.read();
        let mut out = store.convs.get(username).cloned().unwrap_or_default();
        out.sort_by_key(|m| (m.create_time, m.local_id));
        out
    }

    /// 全部联系人，按 `username` 升序。
    pub fn contacts(&self) -> Vec<Contact> {
        let store = self.store.read();
        let mut out: Vec<Contact> = store.contacts.values().cloned().collect();
        out.sort_by(|a, b| a.username.cmp(&b.username));
        out
    }

    /// 会话类型（私聊 / 群 / 公众号 / 其他）。
    pub fn session_kind(&self, username: &str) -> SessionKind {
        SessionKind::classify(username)
    }

    /// 一个发送者在某个群里的**群名片**（不是联系人备注 —— 两者是不同字段）。
    pub fn group_card(&self, chatroom: &str, sender: &str) -> String {
        self.store.read().group_card(Some(chatroom), sender)
    }

    /// 一个群的群主 wxid（取不到时为空串）。
    pub fn chatroom_owner(&self, chatroom: &str) -> String {
        self.store
            .read()
            .chatroom_owner
            .get(chatroom)
            .cloned()
            .unwrap_or_default()
    }
}
