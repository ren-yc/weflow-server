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
    ///
    /// 返回副本（会话是摘要级数据，几百到几千条，代价可接受）。
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
    ///
    /// **返回的是副本，且是整段会话** —— 大群可能是几千条。只要最近几条的调用方，自己 `take`
    /// 即可，但代价已经付过了（这个面刻意不引入分页：分页状态该由调用方持有，而它想要的
    /// 切片方式未必和我们猜的一样）。
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

/// 增量同步的嵌入者视图（需要 `sync` feature）。
///
/// ## 为什么不是直接给出 `sync::AccountSync`
///
/// 那个类型带 `pub` 的 `store` 字段（内部模块要写它）—— 放出去等于把索引的内部布局变成契约，
/// 而这正是承诺面要避免的。它还有 `stop_flag` / `source_files` / `export_media_batch` 这些
/// 服务层自己要用的东西。
///
/// 这里只留嵌入者**驱动更新**所需的那几样，而且事件走 [`Sync::drain_events`] 而不是 tokio 的
/// `broadcast` —— 后者要求调用方处理 `RecvError::Lagged`，那是实现细节，不该是使用者的负担。
///
/// ## 典型用法
///
/// ```no_run
/// use std::path::Path;
/// use std::time::Duration;
/// use weflow_server::api;
///
/// # fn main() -> anyhow::Result<()> {
/// let storage = Path::new("/path/to/<账号>/db_storage");
/// // 真实用法：密钥表由调用方提供（本 crate 不做密钥提取）。
/// let keys = api::KeyMap::Empty;
/// let mut sync = api::Sync::open(storage, &keys, "wxid_…")?;   // 首次全量
/// loop {
///     std::thread::sleep(Duration::from_secs(1));
///     sync.poll_once()?;                    // 增量
///     for ev in sync.drain_events() {       // 事件是**提示**，不是数据
///         let _ = ev;
///     }
///     let _ = sync.index().sessions();      // 读
/// }
/// # }
/// ```
#[cfg(feature = "sync")]
pub struct Sync {
    inner: crate::sync::AccountSync,
    store: Arc<RwLock<Store>>,
}

#[cfg(feature = "sync")]
impl Sync {
    /// 建索引并返回一个可继续增量的句柄。**同步**：真实账号是秒级到十几秒。
    ///
    /// 与 [`open`] 的区别是它**留着**同步引擎 —— 想要「一次读完就走」的用 [`open`]，想要
    /// 持续跟进的用这个。
    pub fn open(
        storage: &std::path::Path,
        keys: &KeyMap,
        my_wxid: &str,
    ) -> anyhow::Result<Self> {
        let store = Arc::new(RwLock::new(Store::default()));
        let mut inner = crate::sync::AccountSync::new(my_wxid, storage, keys.clone(), store.clone());
        inner.full_sync()?;
        Ok(Self { inner, store })
    }

    /// 读当前索引。与 [`Sync`] 共享同一份数据，`poll_once` 之后立刻可见。
    pub fn index(&self) -> Index {
        Index::new(self.store.clone())
    }

    /// 跑一轮增量：返回 `(新增消息数, 撤回数)`。
    ///
    /// 没有变化时是廉价的（只比对库文件的时间戳）。**由调用方决定节奏** —— 本 crate 不替你起
    /// 后台线程，因为「多久轮一次」取决于你要多快看到新消息，而那只有你知道。
    pub fn poll_once(&mut self) -> anyhow::Result<(usize, usize)> {
        self.inner.poll_once()
    }

    /// 取走积压的事件，见 [`crate::sync::AccountSync::drain_events`]。
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.inner.drain_events()
    }
}

// 事件类型属于承诺面：嵌入者要能匹配 `Event::New(..)` 才用得上 `drain_events`。
#[cfg(feature = "sync")]
pub use crate::sync::{Event, NewMessageEvent, PushMedia, RevokeEvent};
