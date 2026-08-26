//! Real-time sync engine (+ event broadcasting) for one account.
//!
//! Modeled on qqflow-server's two-phase design, WeChat-flavored:
//! - a watcher (notify, see `watch.rs`) or a slow fallback timer triggers
//!   `poll_once()`
//! - `poll_once` refreshes the mirror (only changed files re-decrypt), then
//!   reads new rows past each table watermark, applies them to the shared
//!   `Store` and broadcasts `message.new` / `message.revoke` events
//! - read phase never touches the store; apply phase takes the write lock
//!   once, so a failed read leaves the store untouched (no duplicates)

pub mod watch;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::db::mirror::Mirror;
use crate::db::open;
use crate::db::scan::{self, DbFile, DbKind};
use crate::keystore::{DbKey, KeyMap};
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

/// One account's sync engine.
pub struct AccountSync {
    pub wxid: String,
    pub store: Arc<RwLock<Store>>,
    pub events: broadcast::Sender<Event>,
    mirror: Mirror,
    keys: KeyMap,
    /// Live source databases root (`<account>/db_storage`).
    pub storage: PathBuf,
    /// Live source files (re-scanned on each poll; cheap metadata only).
    last_files: Vec<DbFile>,
    /// rel paths that are known to be unopenable (e.g. no key registered) —
    /// reported once, then skipped by polling.
    keyless: std::collections::HashSet<String>,
}

impl AccountSync {
    pub fn new(
        wxid: &str,
        storage: &Path,
        mirror_root: &Path,
        keys: KeyMap,
        store: Arc<RwLock<Store>>,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            mirror: Mirror::new(mirror_root, wxid),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            keyless: std::collections::HashSet::new(),
        }
    }

    pub fn with_channel(
        wxid: &str,
        storage: &Path,
        mirror_root: &Path,
        keys: KeyMap,
        store: Arc<RwLock<Store>>,
        events: broadcast::Sender<Event>,
    ) -> Self {
        AccountSync {
            wxid: wxid.to_string(),
            store,
            events,
            mirror: Mirror::new(mirror_root, wxid),
            keys,
            storage: storage.to_path_buf(),
            last_files: Vec::new(),
            keyless: std::collections::HashSet::new(),
        }
    }

    /// Decrypted snapshot root for this account (`<mirror>/<wxid>/`).
    pub fn snapshot_root(&self) -> PathBuf {
        self.mirror.root.clone()
    }

    /// Full build: refresh everything, rebuild the index from scratch.
    /// Returns the number of (re)decrypted files.
    pub fn full_sync(&mut self) -> Result<usize> {
        let files = self.rescan();
        let (changed, errors) = self.mirror.refresh(&files, &self.keys);
        for (rel, e) in &errors {
            tracing::warn!("mirror error for {rel}: {e}");
        }
        if errors
            .iter()
            .any(|(rel, _)| rel.contains("session.db") || rel.starts_with("session/"))
        {
            tracing::warn!(
                "session db failed to decrypt; check that the registered key matches this account"
            );
        }
        let store = index::build_all(&self.mirror.root, &self.wxid, &files)?;
        *self.store.write() = store;
        Ok(changed.len())
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

    /// Incremental poll: re-decrypt changed files, read new rows past the
    /// watermarks, apply to the store, emit events. Returns the number of new
    /// messages applied.
    pub fn poll_once(&mut self) -> Result<(usize, usize)> {
        let files = self
            .rescan()
            .into_iter()
            .filter(|f| !self.keyless.contains(&f.rel))
            .collect::<Vec<_>>();
        // phase 1: mirror refresh (read-only wrt the store)
        let (changed, errors) = self.mirror.refresh(&files, &self.keys);
        for (rel, e) in &errors {
            // report a per-file problem only once (e.g. keyless dbs), then
            // stop hammering it on every poll
            if self.keyless.insert(rel.clone()) {
                tracing::warn!("mirror error for {rel}: {e}");
            } else {
                tracing::trace!("mirror error (repeated) for {rel}: {e}");
            }
        }
        if changed.is_empty() {
            return Ok((0, 0));
        }

        // phase 1.5: moments timeline refresh when the sns snapshot changed
        if let Some(f) = files
            .iter()
            .find(|f| f.kind == DbKind::Sns && changed.iter().any(|c| c == &f.rel))
        {
            let snap = self.mirror.snapshot_path(&f.rel);
            if let Ok(conn) = open::open_snapshot(&snap) {
                let mut store = self.store.write();
                let _ = index::load_sns(&conn, &mut store);
            }
        }

        // phase 2: incremental reads per changed message/session database
        let mut new_rows: Vec<(String, MessageRecord)> = Vec::new();
        let mut new_watermarks: Vec<(String, Watermark)> = Vec::new();
        let mut revoke_rows: Vec<(String, MessageRecord)> = Vec::new();
        for f in files.iter().filter(|f| f.kind == DbKind::Message || f.kind == DbKind::Session) {
            if !changed.iter().any(|c| c == &f.rel) {
                continue;
            }
            let snap = self.mirror.snapshot_path(&f.rel);
            let Ok(conn) = open::open_snapshot(&snap) else {
                continue;
            };
            let name2id = index::name2id_table(&conn);
            for (table, md5_suffix) in index::message_tables(&conn) {
                let wm_key = format!("{}:{table}", f.rel);
                let wm = {
                    let guard = self.store.read();
                    guard.watermarks.get(&wm_key).copied().unwrap_or_default()
                };
                let rows = read_new(&conn, &table, &wm, name2id.as_deref())?;
                if rows.is_empty() {
                    continue;
                }
                // find the session username for this table
                let session_username = {
                    let guard = self.store.read();
                    index::resolve_table_session(&guard, &md5_suffix)
                };
                for row in rows {
                    if matches!(row.local_type, 10000 | 10002)
                        || row.parsed.revoke.is_some()
                    {
                        revoke_rows.push((session_username.clone(), row));
                    } else {
                        new_rows.push((session_username.clone(), row));
                    }
                }
                let last = new_rows
                    .iter()
                    .chain(revoke_rows.iter())
                    .filter(|(s, _)| *s == session_username)
                    .map(|(_, r)| Watermark {
                        create_time: r.create_time,
                        sort_seq: r.sort_seq,
                        local_id: r.local_id,
                    })
                    .max()
                    .unwrap_or(wm);
                new_watermarks.push((wm_key, last));
            }
        }

        // phase 3: apply (single write lock)
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
                // resolve display name
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
                // only push incoming messages
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
                if row.parsed.revoke.is_none() {
                    // still a system message; skip push
                    continue;
                }
                if row.create_time == 0 {
                    continue;
                }
                // find the withdrawn original
                let original = find_original(&guard, &session, &row);
                let rawid = original
                    .as_ref()
                    .map(|o| o.server_id.to_string())
                    .or_else(|| {
                        row.parsed
                            .revoke
                            .as_ref()
                            .and_then(|r| r.msg_id.clone())
                    })
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
                    format!(
                        "对方撤回了一条消息（rawid：{rawid}） 内容为\"{original_content}\""
                    )
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
        Ok((applied_new, applied_revoke))
    }
}

/// Locate the withdrawn original message inside a conversation.
/// Candidates: server_id == msgid/newmsgid, else local_id == msgid,
/// else the nearest preceding row within 5 minutes.
fn find_original(
    store: &Store,
    session: &str,
    revoke: &MessageRecord,
) -> Option<MessageRecord> {
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