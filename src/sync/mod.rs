//! Real-time sync engine (+ event broadcasting) for one account.
//!
//! qqflow-style **live acquisition**: long-lived read-only SQLCipher
//! connections straight to WeChat's encrypted databases (no mirror, no
//! plaintext on disk). A watcher (`watch.rs`) or a slow fallback timer
//! triggers `poll_once()`:
//!
//! 1. re-enumerate source files; detect changed `(db, wal)` stamp pairs
//! 2. for each changed database, read rows past its table watermarks
//!    directly through the pooled live connection
//! 3. apply to the shared `Store` under one write lock and broadcast
//!    `message.new` / `message.revoke` events
//!
//! Read phase never touches the store; apply phase takes the write lock
//! once, so a failed read leaves the store untouched (no duplicates).
//!
//! Concurrency contract with the live WeChat client: connections are
//! READ_ONLY + `query_only`, WAL lets readers run while WeChat writes, and
//! we never hold transactions across polls (so checkpoints are never blocked
//! by us).

pub mod watch;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::db::live::{AcquireError, LivePool};
use crate::db::scan::{self, DbFile, DbKind};
use crate::keystore::KeyMap;
use crate::store::index::{self, read_new};
use crate::store::{MessageRecord, Store, Watermark};

/// Events broadcast to SSE subscribers (and consumed by tests).
#[derive(Debug, Clone)]
/// 推送给订阅者的事件。
///
/// **事件是提示，不是数据。** 它不保证送达（队列有界、服务会重启），也不保证顺序完整 ——
/// 收到 `New` 的正确反应是**去读那一页**，而不是把事件内容当权威。规范对事件通道也是这个
/// 定位（「不假设可靠送达」）。
pub enum Event {
    /// 连接基线：当前的各表水位线。**连接建立时发一次**，用来让订阅者知道自己从哪开始。
    Sync(Vec<(String, Watermark)>),
    /// 有新消息。
    New(NewMessageEvent),
    /// 有消息被撤回。
    Revoke(RevokeEvent),
}

/// Media metadata pushed over SSE.
///
/// Deliberately excludes:
/// - `aes_key` — a decryption secret, never a metadata field
/// - `url` / `localPath` — media BYTES go through the REST export path
///   (`/api/v1/messages?media=1`), so pushing empty placeholders here would
///   only make clients think a fetchable link exists
#[derive(Debug, Clone)]
pub struct PushMedia {
    /// 媒体大类（`image` / `video` / `voice` / `file` …）。
    pub kind: &'static str,
    /// **导出子目录**（`images` / `voices` / …）；`None` = 这个类型不参与导出。
    ///
    /// 它与 `kind` **不是同一个字符串**（`image` vs `images`），所以只能取自同一处映射
    /// （`media::export::kind_dir_for`）——写两遍必然漂移，而漂移的表现是「导出到了
    /// `images/`、查找却去 `image/`」，两边都静默。
    pub kind_dir: Option<&'static str>,
    /// 建议的文件名，可直接用于导出路径的最后一段。
    pub file_name: String,
    /// 原文件的 md5；取不到时为 `None`。
    pub md5: Option<String>,
}

impl From<&crate::parser::MediaHint> for PushMedia {
    fn from(m: &crate::parser::MediaHint) -> Self {
        Self {
            kind: m.kind.as_str(),
            kind_dir: crate::media::export::kind_dir_for(m.kind),
            file_name: m.file_name.clone(),
            md5: m.md5.clone(),
        }
    }
}

/// 一条新消息的通知。
#[derive(Debug, Clone)]
pub struct NewMessageEvent {
    /// 所属会话（私聊是对方 wxid，群是 `…@chatroom`）。
    pub session_id: String,
    /// 会话类型（`private` / `group` / …），与会话列表里的取值一致。
    pub session_type: &'static str,
    /// 消息的平台 id，可用它回查这一条。
    pub rawid: String,
    /// 发送者的展示名。
    pub source_name: String,
    /// 群名；私聊时为 `None`。
    pub group_name: Option<String>,
    /// 消息正文（或 `[图片]` 一类的占位）。
    pub content: String,
    /// 发送时刻（秒）。
    pub timestamp: i64,
    /// Media metadata when the message carries any (image/voice/video/…).
    pub media: Option<PushMedia>,
}

/// 一次撤回的通知。
///
/// `rawid` 是**被撤回那条消息**的 id —— 用它去索引里找原文（撤回不会删库里的行）。
#[derive(Debug, Clone)]
pub struct RevokeEvent {
    /// 所属会话。
    pub session_id: String,
    /// 会话类型。
    pub session_type: &'static str,
    /// 被撤回消息的平台 id。
    pub rawid: String,
    /// 撤回者的展示名。
    pub source_name: String,
    /// 群名；私聊时为 `None`。
    pub group_name: Option<String>,
    /// 系统消息文本（「xxx 撤回了一条消息」）。
    pub content: String,
    /// 撤回时刻（秒）。
    pub timestamp: i64,
}

/// Source fingerprint for one database: main file + wal sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SrcStamp {
    mtime_ns: i128,
    size: u64,
}

/// Paired source fingerprint: main db + optional wal sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DbStamps {
    main: Option<SrcStamp>,
    wal: Option<SrcStamp>,
}

fn src_stamp(path: &Path) -> Option<SrcStamp> {
    let md = std::fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    Some(SrcStamp { mtime_ns, size: md.len() })
}

enum Work {
    Messages(DbFile),
    Sessions(DbFile),
    Sns(DbFile),
    /// `contact.db` 变了：联系人**以及**群主/名册（后两者也住这个库）。
    Contacts(DbFile),
    /// `contact_fts.db` 变了：群名片。
    GroupMeta(DbFile),
}

impl Work {
    fn file(&self) -> &DbFile {
        match self {
            Work::Messages(f)
            | Work::Sessions(f)
            | Work::Sns(f)
            | Work::Contacts(f)
            | Work::GroupMeta(f) => f,
        }
    }
}

/// One account's sync engine.
pub struct AccountSync {
    pub wxid: String,
    pub store: Arc<RwLock<Store>>,
    /// 内部事件总线。服务层的 SSE 直接订阅它 —— **这不是承诺面**：它要求调用方用 tokio 的
    /// `broadcast` 并处理 `RecvError::Lagged`，而嵌入者不该被绑到这两件事上。
    pub(crate) events: broadcast::Sender<Event>,
    /// 给嵌入者的事件队列，见 [`AccountSync::drain_events`]。
    events_rx: broadcast::Receiver<Event>,
    pool: LivePool,
    keys: KeyMap,
    /// Live source databases root (`<account>/db_storage`).
    pub storage: PathBuf,
    /// Live source files (re-scanned on each poll; cheap metadata only).
    last_files: Vec<DbFile>,
    /// rel -> (main stamp, wal stamp) as of the last successful poll.
    stamps: std::collections::HashMap<String, DbStamps>,
    /// Set when this account is deregistered: every remaining store write in
    /// the current pass is skipped and no further events are emitted.
    ///
    /// Shared with the owning `AccountHandle` as an `Arc` so deregistration can
    /// set it WITHOUT taking this struct's mutex, which a `full_sync` on a real
    /// account holds for minutes.
    stopped: Arc<AtomicBool>,
}

impl AccountSync {
    pub fn new(wxid: &str, storage: &Path, keys: KeyMap, store: Arc<RwLock<Store>>) -> Self {
        let (events, _) = broadcast::channel(1024);
        let events_rx = events.subscribe();
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            events_rx,
            pool: LivePool::new(),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            stamps: std::collections::HashMap::new(),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_channel(
        wxid: &str,
        storage: &Path,
        keys: KeyMap,
        store: Arc<RwLock<Store>>,
        events: broadcast::Sender<Event>,
    ) -> Self {
        let events_rx = events.subscribe();
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            events_rx,
            pool: LivePool::new(),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            stamps: std::collections::HashMap::new(),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 取走自上次调用以来积压的事件（按发生顺序）。
    ///
    /// 嵌入者用它消费增量，**不需要**接触 tokio 的 `broadcast`，也不需要处理 `Lagged`：
    /// 队列满了就丢最旧的（与内部总线同样是有界队列），返回的就是还在的那些。
    ///
    /// 与「读游标」相比，这个接口不需要调用方维护任何状态 —— 取走即消费。代价是**事件不保证
    /// 送达**（服务重启、队列溢出都会丢），所以调用方应当把它当**提示**：收到 `message.new`
    /// 就去读那一页，而不是把事件本身当作数据。规范对事件通道也是这个定位（「不假设可靠
    /// 送达」）。
    ///
    /// 为什么不是 `&self`：它要动接收端的游标。
    pub fn drain_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match self.events_rx.try_recv() {
                Ok(ev) => out.push(ev),
                // `Lagged` 说明调用方太慢，中间的事件已被丢弃 —— 剩下的仍然取走。
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        out
    }

    /// Handle on the retirement flag, for the owning `AccountHandle`.
    ///
    /// There is deliberately no `stop()` here: deregistration sets the flag
    /// through this `Arc` precisely BECAUSE it must not take this struct's
    /// mutex, which a `full_sync` can hold for minutes.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Full build: read every database through the live pool and rebuild the
    /// index from scratch. Returns the number of databases processed.
    pub fn full_sync(&mut self) -> Result<usize> {
        let files = self.rescan();
        let keys = self.keys.clone();
        let store = index::build_all_live(&mut self.pool, &keys, &self.wxid, &files)?;
        // A build on a real account runs for minutes; a deregistration in that
        // window already cleared the store, so installing this index would
        // resurrect it. The caller checks the same flag before flipping to
        // `ready`, so reporting success here is harmless.
        if self.is_stopped() {
            return Ok(files.len());
        }
        {
            let mut guard = self.store.write();
            *guard = store;
            guard.mark_index_built();
        }
        // seed stamps so the next poll starts from a clean baseline
        // 只给**成功打开过**的文件记基线：构建期间打不开的库（缺 key / busy）
        // 若也被记上戳，poll 会把它的存量数据判成「未变」—— 存量就被卡死到
        // 文件下次变化为止。没记基线的文件 classify 必判变，首轮 poll 即补读。
        self.stamps.clear();
        for f in &files {
            if !self.pool.is_open(&f.rel) {
                continue;
            }
            let main = src_stamp(&f.abs);
            let wal = f.wal.as_deref().and_then(src_stamp);
            self.stamps.insert(
                f.rel.clone(),
                DbStamps {
                    main: Some(main.unwrap_or(SrcStamp { mtime_ns: 0, size: 0 })),
                    wal,
                },
            );
        }
        Ok(files.len())
    }

    /// Re-enumerate the live source databases.
    fn rescan(&mut self) -> Vec<DbFile> {
        self.last_files = scan::enum_db_files(&self.storage);
        self.last_files.clone()
    }

    /// Get the current source files (used by the watcher to know what changed).
    pub fn source_files(&self) -> &[DbFile] {
        &self.last_files
    }

    fn classify_changed(&mut self) -> Vec<Work> {
        let files = self.rescan();
        let mut work: Vec<Work> = Vec::new();
        for f in &files {
            // 两级检测：**已打开**的文件走 `data_version`（别的连接提交后必然
            // 变化，与文件系统时间戳无关 —— Windows 高负载下 mtime/size 回读
            // 滞后不会再把一次写入误判成「未变」）；**未打开**的文件没有连接，
            // 只能继续用 mtime/size 戳做前置门。无基线（首见文件）一律判变。
            if self.pool.is_open(&f.rel) {
                // 只**窥探**不推进基线：这一轮若因无 key / 打开失败而读不到数据，
                // 基线必须保持原样，否则变更同样会被吞掉（换了个机制的同一 bug）。
                let changed = self.pool.foreign_commit_pending(&f.rel).unwrap_or(true);
                if !changed {
                    continue;
                }
            } else {
                let main = src_stamp(&f.abs);
                let wal = f.wal.as_deref().and_then(src_stamp);
                let cur = DbStamps { main, wal };
                let unchanged = self
                    .stamps
                    .get(&f.rel)
                    .is_some_and(|prev| *prev == cur);
                if unchanged {
                    continue;
                }
            }
            match f.kind {
                DbKind::Message => work.push(Work::Messages(f.clone())),
                DbKind::Session => work.push(Work::Sessions(f.clone())),
                DbKind::Sns => work.push(Work::Sns(f.clone())),
                DbKind::Contact => work.push(Work::Contacts(f.clone())),
                // 群元数据也要跟着变：名片住在 fts 库里，名册/群主住在 contact.db 里。
                // 单独一类是必要的 —— 原来 fts 库被归为 `Contact`，于是「名片变了」会去
                // **重载联系人**，而真正该重载的东西没人管。
                DbKind::ContactFts => work.push(Work::GroupMeta(f.clone())),
                _ => {}
            }
        }
        work
    }

    /// Incremental poll: for databases whose (db, wal) stamps changed, run
    /// watermark-increment reads on their live connections, apply to the
    /// store and broadcast events. Returns (new_messages, revokes).
    pub fn poll_once(&mut self) -> Result<(usize, usize)> {
        if self.is_stopped() {
            return Ok((0, 0));
        }
        let work = self.classify_changed();
        if work.is_empty() {
            return Ok((0, 0));
        }

        // phase 1: incremental reads (read-only wrt the store)
        let mut new_rows: Vec<(String, MessageRecord)> = Vec::new();
        let mut new_watermarks: Vec<(String, Watermark)> = Vec::new();
        let mut revoke_rows: Vec<(String, MessageRecord)> = Vec::new();
        // 本轮**真正读过数据**的文件：只有它们才允许在 phase 4 记账
        // （戳与 data_version 基线）。读失败/无 key/打开失败的文件不记账，
        // 下一轮 classify 才会把它再当「已变更」重查。
        let mut read_done: Vec<String> = Vec::new();

        for w in &work {
            match w {
                Work::Sessions(_) | Work::Contacts(_) | Work::GroupMeta(_) | Work::Sns(_) => {}
                Work::Messages(f) => {
                    let Some(key) = self.keys.key_for(&f.rel) else {
                        continue;
                    };
                    let conn = match self.pool.get_or_open(f, key) {
                        Ok(c) => c,
                        Err(AcquireError::WrongKey) => {
                            // 确定性失败进退避：busy_timeout 5 秒 × watch 350ms
                            // 高频触发会让坏 key 的文件拖垮整个 poll 节奏。
                            // 瞬时 Io 失败（下一分支）保持逐轮重试 —— 那类失败
                            // 不记戳，天然就是「下一轮重查」。
                            tracing::warn!("live open failed for {} (wrong key?)", f.rel);
                            self.pool.mark_keyless(&f.rel);
                            continue;
                        }
                        Err(e) => {
                            tracing::debug!("live open deferred for {}: {e}", f.rel);
                            continue;
                        }
                    };
                    let name2id = index::name2id_table(conn);
                    for (table, md5_suffix) in index::message_tables(conn) {
                        let wm_key = format!("{}:{table}", f.rel);
                        let wm = {
                            let guard = self.store.read();
                            guard.watermarks.get(&wm_key).copied().unwrap_or_default()
                        };
                        let rows = read_new(conn, &table, &wm, name2id.as_deref())?;
                        for row in rows {
                            let session_username = {
                                let guard = self.store.read();
                                index::resolve_table_session(&guard, &md5_suffix)
                            };
                            let wm = Watermark {
                                create_time: row.create_time,
                                sort_seq: row.sort_seq,
                                local_id: row.local_id,
                            };
                            // Mask before matching: `local_type` is packed
                            // (`(subtype << 32) | base`). System codes carry a
                            // zero high half in practice, so this is a no-op on
                            // today's data, but the bare comparison is the same
                            // latent bug that killed the parser's appmsg branch.
                            let (base_type, _) =
                                crate::parser::split_local_type(row.local_type);
                            if matches!(base_type, 10000 | 10002)
                                || row.parsed.revoke.is_some()
                            {
                                revoke_rows.push((session_username.clone(), row));
                            } else {
                                new_rows.push((session_username.clone(), row));
                            }
                            new_watermarks.push((wm_key.clone(), wm));
                        }
                    }
                    read_done.push(f.rel.clone());
                }
            }
        }

        // phase 2: apply (single write lock) and emit events
        let mut applied_new = 0usize;
        let mut applied_revoke = 0usize;
        {
            let mut guard = self.store.write();
            // Deregistered while phase 1 was reading: discard the rows instead
            // of writing them into the just-cleared store, and emit nothing —
            // the bus is process-wide, so events for a gone account would
            // reach every subscriber.
            if self.stopped.load(Ordering::SeqCst) {
                return Ok((0, 0));
            }
            let my_wxid = guard.my_wxid.clone();
            for (wm_key, wm) in new_watermarks {
                guard.watermarks.insert(wm_key, wm);
            }
            for (session, row) in new_rows {
                let is_send = !row.sender_username.is_empty() && row.sender_username == my_wxid;
                let sender_name = guard
                    .contacts
                    .get(&row.sender_username)
                    .map(|c| c.display_name())
                    .filter(|s| !s.is_empty() && s != &row.sender_username)
                    .unwrap_or_else(|| row.sender_username.clone());
                let conv = guard.convs.entry(session.clone()).or_default();
                conv.push(MessageRecord {
                    sender_name: sender_name.clone(),
                    is_send,
                    ..row
                });
                if !is_send {
                    applied_new += 1;
                    let ev = NewMessageEvent {
                        session_id: session.clone(),
                        session_type: guard
                            .sessions
                            .get(&session)
                            .map(|s| s.kind.as_str())
                            .unwrap_or("other"),
                        rawid: guard
                            .convs
                            .get(&session)
                            .and_then(|v| v.last())
                            .map(|r| r.server_id.to_string())
                            .unwrap_or_default(),
                        source_name: sender_name,
                        group_name: guard.sessions.get(&session).map(|s| s.kind.as_str()).map(|kind| {
                            if kind == "group" {
                                guard.session_display_opt(&session).unwrap_or_default()
                            } else {
                                String::new()
                            }
                        }),
                        content: guard
                            .convs
                            .get(&session)
                            .and_then(|v| v.last())
                            .map(|r| r.parsed.display.clone())
                            .unwrap_or_default(),
                        timestamp: guard
                            .convs
                            .get(&session)
                            .and_then(|v| v.last())
                            .map(|r| r.create_time)
                            .unwrap_or_default(),
                        // Metadata only — see `PushMedia`. Without this the
                        // push path gave no media hint at all and clients had
                        // to re-query REST just to learn a message had media.
                        media: guard
                            .convs
                            .get(&session)
                            .and_then(|v| v.last())
                            .and_then(|r| r.parsed.media.as_ref())
                            .map(PushMedia::from),
                    };
                    let _ = self.events.send(Event::New(ev));
                }
            }
            for (session, row) in revoke_rows {
                if row.parsed.revoke.is_none() || row.create_time == 0 {
                    continue;
                }
                let original = find_original(&guard, &session, &row);
                let rawid = original
                    .as_ref()
                    .map(|o| o.server_id.to_string())
                    .or_else(|| row.parsed.revoke.as_ref().and_then(|r| r.msg_id.clone()))
                    .unwrap_or_default();
                let original_content = original
                    .as_ref()
                    .map(|o| o.parsed.display.clone())
                    .unwrap_or_default();
                let content = if original_content.is_empty() {
                    row.parsed
                        .revoke
                        .as_ref()
                        .and_then(|r| r.replace_msg.clone())
                        .unwrap_or_else(|| "对方撤回了一条消息".to_string())
                } else {
                    format!("对方撤回了一条消息（rawid：{rawid}） 内容为\"{original_content}\"")
                };
                applied_revoke += 1;
                let ev = RevokeEvent {
                    session_id: session.clone(),
                    session_type: guard
                        .sessions
                        .get(&session)
                        .map(|s| s.kind.as_str())
                        .unwrap_or("other"),
                    rawid,
                    source_name: row.sender_name.clone(),
                    group_name: guard.sessions.get(&session).map(|s| s.kind.as_str()).map(|kind| {
                        if kind == "group" {
                            guard.session_display_opt(&session).unwrap_or_default()
                        } else {
                            String::new()
                        }
                    }),
                    content,
                    timestamp: row.create_time,
                };
                let _ = self.events.send(Event::Revoke(ev));
            }
        }

        // phase 3: dependent section reloads over warm live connections
        let keys = self.keys.clone();
        for w in &work {
            // Each arm takes its own write lock, so the flag is re-checked per
            // item rather than once for the whole loop.
            if self.is_stopped() {
                return Ok((applied_new, applied_revoke));
            }
            match w {
                Work::Sessions(f) => {
                        let Some(k) = keys.key_for(&f.rel) else { continue };
                     if let Ok(conn) = self.pool.get_or_open(f, k) {
                        let mut store = self.store.write();
                        if let Err(e) = index::load_sessions(conn, &mut store) {
                            tracing::warn!("sessions reload failed: {e}");
                        }
                    }
                }
                Work::Contacts(f) => {
                        let Some(k) = keys.key_for(&f.rel) else { continue };
                     if let Ok(conn) = self.pool.get_or_open(f, k) {
                        let mut store = self.store.write();
                        if let Err(e) = index::load_contacts(conn, &mut store) {
                            tracing::warn!("contacts reload failed: {e}");
                        }
                        // 群主与名册也住 contact.db —— 顺带一起刷，免得它们只在启动时是对的。
                        crate::store::group_meta::load_chatroom_meta(conn, &mut store);
                    }
                }
                Work::GroupMeta(f) => {
                        let Some(k) = keys.key_for(&f.rel) else { continue };
                     if let Ok(conn) = self.pool.get_or_open(f, k) {
                        let mut store = self.store.write();
                        crate::store::group_meta::load_group_cards(conn, &mut store);
                    }
                }
                Work::Sns(f) => {
                        let Some(k) = keys.key_for(&f.rel) else { continue };
                     if let Ok(conn) = self.pool.get_or_open(f, k) {
                        let mut store = self.store.write();
                        if let Err(e) = index::load_sns(conn, &mut store) {
                            tracing::warn!("sns reload failed: {e}");
                        }
                    }
                }
                Work::Messages(_) => {}
            }
        }

        // phase 4: remember stamps for everything we **actually read**.
        // 记账规则与「失败即 continue」是成对的：一个文件这一轮没读到数据
        // （无 key / 打开失败），它的戳就必须保持原样 —— 否则这轮的变更会被
        // 下一轮的「未变」判定永久吞掉。已打开连接的文件改由 data_version
        // 基线盯增量（open 时建立），mtime/size 只服务未打开的文件。
        for w in &work {
            let f = w.file();
            if !read_done.contains(&f.rel) {
                continue;
            }
            let main = src_stamp(&f.abs);
            let wal = f.wal.as_deref().and_then(src_stamp);
            self.stamps.insert(
                f.rel.clone(),
                DbStamps { main, wal },
            );
            // data_version 基线同理：只在**真读过**的文件上推进。
            self.pool.advance_data_version(&f.rel);
        }

        // 只有这一轮确实动过索引才更新时刻：空转的一轮不该让 updatedAt 看起来更新了。
        if applied_new > 0 || applied_revoke > 0 || !work.is_empty() {
            self.store.write().mark_index_built();
        }

        Ok((applied_new, applied_revoke))
    }

    /// Run the media export batch against this account.
    ///
    /// Auxiliary databases (`hardlink`, `media_*`, `emoticon`) are opened
    /// fresh read-only per batch via the raw-key path — cheap (no KDF), and
    /// it keeps the long-lived warm pool exclusively for index/poll traffic.
    pub fn export_media_batch(
        &mut self,
        account_dir: &Path,
        media_keys: Option<crate::keystore::ImageKeys>,
        export_dir: &Path,
        jobs: &[(i64, crate::parser::MediaKind, Option<String>, i64, String)],
        max_items: usize,
    ) -> std::collections::HashMap<i64, crate::media::export::ExportedMedia> {
        crate::media::export::export_batch_live(
            &self.storage,
            &self.keys,
            account_dir,
            export_dir,
            media_keys,
            &self.wxid,
            jobs,
            max_items,
        )
    }
}

/// Locate the withdrawn original message inside a conversation.
/// Candidates: server_id == msgid/newmsgid, else local_id == msgid,
/// else the nearest preceding row within 5 minutes.
fn find_original(store: &Store, session: &str, revoke: &MessageRecord) -> Option<MessageRecord> {
    let ids: Vec<String> = [
        revoke.parsed.revoke.as_ref()?.msg_id.clone(),
        revoke.parsed.revoke.as_ref()?.new_msg_id.clone(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let conv = store.convs.get(session)?;
    for m in conv.iter().rev() {
        if m.local_id == revoke.local_id {
            continue;
        }
        let svr = m.server_id.to_string();
        if ids.contains(&svr) {
            return Some(m.clone());
        }
    }
    // fallback: nearest previous incoming message within 300s
    let mut best: Option<&MessageRecord> = None;
    for m in conv.iter().rev() {
        if m.local_id >= revoke.local_id {
            continue;
        }
        if revoke.create_time - m.create_time > 300 {
            break;
        }
        if !m.is_send && m.parsed.display != "[消息]" {
            best = Some(m);
            break;
        }
    }
    best.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_original_by_server_id_matches_memory_rows() {
        let mut store = Store {
            my_wxid: "me".into(),
            ..Default::default()
        };
        let conv = store.convs.entry("sess".into()).or_default();
        conv.push(MessageRecord {
            local_id: 1,
            server_id: 100,
            local_type: 1,
            create_time: 1000,
            sort_seq: 0,
            is_send: false,
            sender_username: "a".into(),
            sender_name: "a".into(),
            parsed: crate::parser::parse_message(1, 100, 1, "你好"),
        });
        conv.push(MessageRecord {
            local_id: 2,
            server_id: 101,
            local_type: 10002,
            create_time: 1300,
            sort_seq: 0,
            is_send: false,
            sender_username: "a".into(),
            sender_name: "a".into(),
            parsed: crate::parser::parse_message(
                10002,
                101,
                2,
                r#"<sysmsg type="revokemsg"><revokemsg><msgid>100</msgid></revokemsg></sysmsg>"#,
            ),
        });
        let revoke = conv.last().unwrap().clone();
        let found = find_original(&store, "sess", &revoke);
        assert!(found.is_some());
        assert_eq!(found.unwrap().server_id, 100);
    }

    // ---- 廉价跳过的时序竞态回归（修复前各有一条会红）----

    use rusqlite::Connection;

    const KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const WXID: &str = "wxid_fake000000000000001";
    const GROUP: &str = "wxid_fake_group@chatroom";

    fn md5_hex(s: &str) -> String {
        use md5::Digest;
        let mut h = md5::Md5::new();
        h.update(s.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// 造一个**只含 message_0.db** 的加密账号（表结构与真库同形：Name2Id0 +
    /// `Msg_<md5(群)>`），返回 `db_storage` 目录。夹具只能造库，不造索引。
    fn build_message_account(dir: &Path, key: &[u8; 32]) -> PathBuf {
        let storage = dir.join("db_storage");
        std::fs::create_dir_all(storage.join("message")).unwrap();
        std::fs::create_dir_all(storage.join("session")).unwrap();
        // 会话表：消息到会话的解析靠 md5 前缀匹配 sessions 里的 username，
        // 没有 session.db 时会落到 md5 后缀本身 —— 夹具必须带上它。
        {
            let sconn = Connection::open(storage.join("session/session.db")).unwrap();
            let key_hex = hex::encode(key);
            sconn
                .execute_batch(&format!(
                    "PRAGMA cipher_page_size = 4096;\n                 PRAGMA key = \"x'{key_hex}'\";\n                 PRAGMA journal_mode = DELETE;\n                 CREATE TABLE Session (\n                    userName TEXT PRIMARY KEY,\n                    displayName TEXT NOT NULL,\n                    sortTimeStamp INTEGER NOT NULL DEFAULT 0,\n                    lastTimeStamp INTEGER NOT NULL DEFAULT 0,\n                    lastMsg TEXT,\n                    lastMsgType INTEGER NOT NULL DEFAULT 0,\n                    unread INTEGER NOT NULL DEFAULT 0,\n                    type INTEGER NOT NULL DEFAULT 0\n                 );\n                 INSERT INTO Session VALUES ('{GROUP}', '项目群', 1700000015, 1700000015, '[图片]', 3, 0, 2);"
                ))
                .unwrap();
            drop(sconn);
        }
        let conn = Connection::open(storage.join("message/message_0.db")).unwrap();
        let key_hex = hex::encode(key);
        conn.execute_batch(&format!(
            "PRAGMA cipher_page_size = 4096;\n             PRAGMA key = \"x'{key_hex}'\";\n             PRAGMA journal_mode = DELETE;"
        ))
        .unwrap();
        let group_md5 = md5_hex(GROUP);
        conn.execute_batch(&format!(
            "CREATE TABLE \"Name2Id0\" (user_name TEXT);\n             INSERT INTO \"Name2Id0\" (rowid, user_name) VALUES (1, 'a'), (2, 'b');\n             CREATE TABLE \"Msg_{group_md5}\" (\n                local_id INTEGER PRIMARY KEY AUTOINCREMENT,\n                server_id INTEGER NOT NULL,\n                local_type INTEGER NOT NULL,\n                create_time INTEGER NOT NULL,\n                sort_seq INTEGER NOT NULL DEFAULT 0,\n                real_sender_id INTEGER NOT NULL,\n                message_content TEXT,\n                compress_content BLOB\n             );"
        ))
        .unwrap();
        conn.execute(
            &format!(
                "INSERT INTO \"Msg_{group_md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content) VALUES (?1, 1, ?2, 0, 2, '第一层')"
            ),
            rusqlite::params![8_100_000_000_000_000_001i64, 1_700_000_100i64],
        )
        .unwrap();
        drop(conn);
        storage
    }

    fn append_row(storage: &Path, key: &[u8; 32], server_id: i64) {
        let conn = Connection::open(storage.join("message/message_0.db")).unwrap();
        let key_hex = hex::encode(key);
        conn
            .execute_batch(&format!(
                "PRAGMA cipher_page_size = 4096;\n             PRAGMA key = \"x'{key_hex}'\";\n             PRAGMA journal_mode = DELETE;"
            ))
            .unwrap();
        let group_md5 = md5_hex(GROUP);
        conn.execute(
            &format!(
                "INSERT INTO \"Msg_{group_md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content) VALUES (?1, 1, ?2, 0, 2, '新消息')"
            ),
            rusqlite::params![server_id, 1_700_000_200i64],
        )
        .unwrap();
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "weflow-sync-race-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// 候选 1 路：**mtime/size 戳与基线相同**（模拟 Windows 高负载下元数据回读滞后）
    /// 但数据已提交 ⇒ 廉价跳过不得吞掉这一行。修复前：戳相同 ⇒ classify 判「未变」⇒
    /// poll 返回 0 ⇒ 红；修复后：已打开的文件改查 data_version ⇒ 仍能捡到 ⇒ 绿。
    #[test]
    fn stamp_unchanged_but_committed_row_is_still_polled() {
        let dir = unique_dir("stale-stamp");
        let key = crate::keystore::parse_db_key(KEY_HEX).unwrap().0;
        let storage = build_message_account(&dir, &key);
        let store = Arc::new(RwLock::new(Store::default()));
        let mut sync = AccountSync::new(WXID, &storage, crate::keystore::KeyMap::from(crate::keystore::DbKey(key)), store.clone());
        sync.full_sync().unwrap();
        assert_eq!(store.read().convs[GROUP].len(), 1, "基线一行");

        // 模拟微信写入：另一条连接追加一行。
        append_row(&storage, &key, 8_100_000_000_000_000_002);

        // 把该文件的戳改写为**当前真实戳**（等于「写入前后文件系统看起来没变」）。
        let rel = "message/message_0.db";
        {
            let files = scan::enum_db_files(&storage);
            let f = files.iter().find(|f| f.rel == rel).unwrap();
            let main = src_stamp(&f.abs);
            let wal = f.wal.as_deref().and_then(src_stamp);
            sync.stamps.insert(
                rel.to_string(),
                DbStamps { main, wal },
            );
        }

        let (n, _) = sync.poll_once().unwrap();
        assert_eq!(n, 1, "戳相同但数据已提交：必须仍被捡到（不能靠文件戳跳过）");
        assert_eq!(store.read().convs[GROUP].len(), 2);
    }

    /// 候选 2 路：本轮打开失败（WrongKey）⇒ 打开失败的文件**不得记戳**，否则下一轮
    /// 判「未变」把这次变更永久吞掉。修复前：phase 4 无条件重记戳 ⇒ 第二次 poll 返回 0
    /// ⇒ 红；修复后：失败文件的戳保持基线 ⇒ 第二次 poll 重查 ⇒ 绿。
    #[test]
    fn failed_open_does_not_swallow_the_next_poll() {
        let dir = unique_dir("failed-open");
        let key = crate::keystore::parse_db_key(KEY_HEX).unwrap().0;
        let storage = build_message_account(&dir, &key);
        let store = Arc::new(RwLock::new(Store::default()));
        let mut sync = AccountSync::new(WXID, &storage, crate::keystore::KeyMap::from(crate::keystore::DbKey(key)), store.clone());
        sync.full_sync().unwrap();

        // 写入一行（此时文件戳必然变化）。
        append_row(&storage, &key, 8_100_000_000_000_000_003);

        // 让下一轮**读不到这一行**：换上空密钥表 —— 该文件在 phase 1 的
        // 「无 key ⇒ continue」与「打开失败 ⇒ continue」走同一条跳过路径
        // （Windows 下池连接还开着，改名会 EBUSY，密钥注入是等价且可行的构造）。
        // 修复前的失败模式：这一轮结束 phase 4 无条件重记戳 ⇒ 下一轮判「未变」
        // ⇒ 这一行被永久吞掉。修复后：没读过数据的文件不记戳 ⇒ 恢复后重查 ⇒ 捡到。
        sync.keys = crate::keystore::KeyMap::Empty;
        let (n1, _) = sync.poll_once().unwrap();
        assert_eq!(n1, 0, "无密钥轮次读不到行");

        // 恢复正确密钥：这一轮必须把上一轮的行捡回来。
        sync.keys = crate::keystore::KeyMap::Single(crate::keystore::DbKey(key));
        let (n2, _) = sync.poll_once().unwrap();
        assert_eq!(n2, 1, "无密钥的那一轮不得吞掉变更：恢复后必须重查");
        assert_eq!(store.read().convs[GROUP].len(), 2);
    }
}
// touch
