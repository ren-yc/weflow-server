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
pub enum Event {
    /// Connection baseline (current watermarks).
    Sync(Vec<(String, Watermark)>),
    New(NewMessageEvent),
    Revoke(RevokeEvent),
}

#[derive(Debug, Clone)]
pub struct NewMessageEvent {
    pub session_id: String,
    pub session_type: &'static str,
    pub rawid: String,
    pub source_name: String,
    pub group_name: Option<String>,
    pub content: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct RevokeEvent {
    pub session_id: String,
    pub session_type: &'static str,
    pub rawid: String,
    pub source_name: String,
    pub group_name: Option<String>,
    pub content: String,
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
    Contacts(DbFile),
}

impl Work {
    fn file(&self) -> &DbFile {
        match self {
            Work::Messages(f) | Work::Sessions(f) | Work::Sns(f) | Work::Contacts(f) => f,
        }
    }
}

/// One account's sync engine.
pub struct AccountSync {
    pub wxid: String,
    pub store: Arc<RwLock<Store>>,
    pub events: broadcast::Sender<Event>,
    pool: LivePool,
    keys: KeyMap,
    /// Live source databases root (`<account>/db_storage`).
    pub storage: PathBuf,
    /// Live source files (re-scanned on each poll; cheap metadata only).
    last_files: Vec<DbFile>,
    /// rel -> (main stamp, wal stamp) as of the last successful poll.
    stamps: std::collections::HashMap<String, DbStamps>,
}

impl AccountSync {
    pub fn new(wxid: &str, storage: &Path, keys: KeyMap, store: Arc<RwLock<Store>>) -> Self {
        let (events, _) = broadcast::channel(1024);
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            pool: LivePool::new(),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            stamps: std::collections::HashMap::new(),
        }
    }

    pub fn with_channel(
        wxid: &str,
        storage: &Path,
        keys: KeyMap,
        store: Arc<RwLock<Store>>,
        events: broadcast::Sender<Event>,
    ) -> Self {
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            pool: LivePool::new(),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            stamps: std::collections::HashMap::new(),
        }
    }

    /// Full build: read every database through the live pool and rebuild the
    /// index from scratch. Returns the number of databases processed.
    pub fn full_sync(&mut self) -> Result<usize> {
        let files = self.rescan();
        let keys = self.keys.clone();
        let store = index::build_all_live(&mut self.pool, &keys, &self.wxid, &files)?;
        *self.store.write() = store;
        // seed stamps so the next poll starts from a clean baseline
        self.stamps.clear();
        for f in &files {
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
            let main = src_stamp(&f.abs);
            let wal = f.wal.as_deref().and_then(src_stamp);
            let cur = DbStamps {
                main,
                wal,
            };
            let unchanged = self
                .stamps
                .get(&f.rel)
                .map(|prev| *prev == cur)
                .unwrap_or(false);
            if unchanged {
                continue;
            }
            match f.kind {
                DbKind::Message => work.push(Work::Messages(f.clone())),
                DbKind::Session => work.push(Work::Sessions(f.clone())),
                DbKind::Sns => work.push(Work::Sns(f.clone())),
                DbKind::Contact => work.push(Work::Contacts(f.clone())),
                _ => {}
            }
        }
        work
    }

    /// Incremental poll: for databases whose (db, wal) stamps changed, run
    /// watermark-increment reads on their live connections, apply to the
    /// store and broadcast events. Returns (new_messages, revokes).
    pub fn poll_once(&mut self) -> Result<(usize, usize)> {
        let work = self.classify_changed();
        if work.is_empty() {
            return Ok((0, 0));
        }

        // phase 1: incremental reads (read-only wrt the store)
        let mut new_rows: Vec<(String, MessageRecord)> = Vec::new();
        let mut new_watermarks: Vec<(String, Watermark)> = Vec::new();
        let mut revoke_rows: Vec<(String, MessageRecord)> = Vec::new();

        for w in &work {
            match w {
                Work::Sessions(_) | Work::Contacts(_) | Work::Sns(_) => {}
                Work::Messages(f) => {
                    let Some(key) = self.keys.key_for(&f.rel) else {
                        continue;
                    };
                    let conn = match self.pool.get_or_open(f, key) {
                        Ok(c) => c,
                        Err(AcquireError::WrongKey) => {
                            tracing::warn!("live open failed for {} (wrong key?)", f.rel);
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
                            if matches!(row.local_type, 10000 | 10002)
                                || row.parsed.revoke.is_some()
                            {
                                revoke_rows.push((session_username.clone(), row));
                            } else {
                                new_rows.push((session_username.clone(), row));
                            }
                            new_watermarks.push((wm_key.clone(), wm));
                        }
                    }
                }
            }
        }

        // phase 2: apply (single write lock) and emit events
        let mut applied_new = 0usize;
        let mut applied_revoke = 0usize;
        {
            let mut guard = self.store.write();
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
                        group_name: guard.sessions.get(&session).map(|s| {
                            if s.kind.as_str() == "group" {
                                s.display_name.clone()
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
                    group_name: guard.sessions.get(&session).map(|s| {
                        if s.kind.as_str() == "group" {
                            s.display_name.clone()
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

        // phase 4: remember stamps for everything we processed
        for w in &work {
            let f = w.file();
            let main = src_stamp(&f.abs);
            let wal = f.wal.as_deref().and_then(src_stamp);
            self.stamps.insert(
                f.rel.clone(),
                DbStamps { main, wal },
            );
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
}
// touch