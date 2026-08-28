//! Build the in-memory index over live read-only connections, and incremental reads.
//!
//! Every schema assumption is probed at runtime (`PRAGMA table_info`) against
//! candidate column aliases (the same aliases WeFlow consumes), and any
//! missing table degrades to an empty section instead of failing the account.
//! Tables named `Msg_<md5>` / `msg_<md5>` are picked up case-insensitively.


use anyhow::Result;
use rusqlite::{Connection, Row};

use super::{Contact, MessageRecord, Session, SessionKind, Store, Watermark};
use crate::db::live::LivePool;
use crate::db::open;
use crate::keystore::KeyMap;
use crate::db::scan::{DbFile, DbKind};
use crate::parser;

const SESSION_COLUMNS: &[&[&str]] = &[
    &["user_name", "username", "userName", "talker", "talker_id", "session_id"],
    &["display_name", "displayName", "nick_name", "nickname", "name"],
    &["sort_timestamp", "sortTimestamp", "sort_time", "last_msg_time"],
    &["last_timestamp", "lastTimestamp", "last_msg_time", "lastTime"],
    &["summary", "digest", "last_msg", "lastMsg", "last_message"],
    &["last_msg_type", "lastMsgType", "last_type", "lastType"],
    &["unread_count", "unreadCount", "unread", "total_count"],
    &["type", "session_type", "sessionType"],
];

const CONTACT_COLUMNS: &[&[&str]] = &[
    &["user_name", "username", "userName"],
    &["remark", "remark_name", "remarkName"],
    &["nick_name", "nickname", "nickName"],
    &["alias", "wx_id", "wxid", "alias_name"],
    &["avatar_url", "avatar", "head_img_url", "headImageUrl"],
    &["local_type", "localType", "type", "user_type", "userType"],
];

const MSG_COLUMNS: &[&[&str]] = &[
    &["local_id", "localId", "id"],
    &["server_id", "serverId", "msg_svr_id", "msgSvrId"],
    &["local_type", "localType", "type", "msg_type", "msgType"],
    &["create_time", "createTime", "msg_create_time", "timestamp"],
    &["sort_seq", "sortSeq", "seq", "msg_seq"],
    &["sender_username", "senderUsername", "talker", "from_username"],
    &["real_sender_id", "realSenderId", "sender_rowid", "send_id"],
];

/// Case-insensitive first match of an alias group against table columns.
fn pick<'a>(cols: &'a [String], group: &[&str]) -> Option<&'a str> {
    for alias in group {
        for c in cols {
            if c.eq_ignore_ascii_case(alias) {
                return Some(c);
            }
        }
    }
    None
}

fn get_opt_str(row: &Row, idx: usize) -> Option<String> {
    row.get::<_, Option<String>>(idx).ok().flatten().map(|s| s.trim().to_string())
}

fn get_i64(row: &Row, idx: usize) -> i64 {
    row.get::<_, i64>(idx).unwrap_or(0)
}

fn get_opt_blob(row: &Row, idx: usize) -> Option<Vec<u8>> {
    row.get::<_, Option<Vec<u8>>>(idx).ok().flatten()
}

/// Raw column bytes, accepting both TEXT and BLOB storage classes.
fn get_opt_bytes(row: &Row, idx: usize) -> Option<Vec<u8>> {
    match row.get_ref(idx) {
        Ok(rusqlite::types::ValueRef::Text(b)) | Ok(rusqlite::types::ValueRef::Blob(b)) => {
            Some(b.to_vec())
        }
        _ => None,
    }
}

/// Parse the `Session` table of `session.db` into the sessions map.
pub fn load_sessions(conn: &Connection, store: &mut Store) -> Result<()> {
    let table = if open::table_columns(conn, "SessionTable").is_empty() {
        let candidates = open::table_names(conn, 50);
        match candidates
            .iter()
            .find(|t| t.eq_ignore_ascii_case("SessionTable"))
            .or_else(|| candidates.iter().find(|t| t.eq_ignore_ascii_case("Session")))
            .or_else(|| candidates.iter().find(|t| t.eq_ignore_ascii_case("session_table")))
        {
            Some(t) => t.clone(),
            None => {
                tracing::warn!("session.db has no SessionTable/Session table; degrade to message-only index");
                return Ok(());
            }
        }
    } else {
        "SessionTable".to_string()
    };
    load_sessions_from(conn, &table, store)
}

fn load_sessions_from(conn: &Connection, table: &str, store: &mut Store) -> Result<()> {
    let cols = open::table_columns(conn, table);
    let c_username = pick(&cols, SESSION_COLUMNS[0]);
    let c_name = pick(&cols, SESSION_COLUMNS[1]);
    let c_last = pick(&cols, SESSION_COLUMNS[3]);
    let c_summary = pick(&cols, SESSION_COLUMNS[4]);
    let c_last_type = pick(&cols, SESSION_COLUMNS[5]);
    let c_unread = pick(&cols, SESSION_COLUMNS[6]);
    let c_type = pick(&cols, SESSION_COLUMNS[7]);
    let Some(c_username) = c_username else {
        return Ok(()); // unusable table: degrade
    };
    let selected: Vec<&str> = [
        Some(c_username),
        c_name,
        c_last,
        c_summary,
        c_last_type,
        c_unread,
        c_type,
    ]
    .into_iter()
    .flatten()
    .collect();
    let query = format!("SELECT {} FROM \"{table}\"", selected.join(", "));
    let mut stmt = conn.prepare(&query)?;
    let has = [c_name.is_some(), c_last.is_some(), c_summary.is_some(), c_last_type.is_some(), c_unread.is_some(), c_type.is_some()];
    // position of the n-th optional column (n counts among the optionals)
    let pos = |n: usize| -> usize { 1 + has[..n].iter().filter(|b| **b).count() };
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let username = get_opt_str(row, 0).unwrap_or_default();
        if username.is_empty() {
            continue;
        }
        let display_name = if c_name.is_some() {
            get_opt_str(row, pos(0)).unwrap_or_default()
        } else {
            String::new()
        };
        let last_timestamp = if c_last.is_some() { get_i64(row, pos(1)) } else { 0 };
        let summary = if c_summary.is_some() { get_opt_str(row, pos(2)) } else { None };
        let last_msg_type = if c_last_type.is_some() { Some(get_i64(row, pos(3))) } else { None };
        let unread_count = if c_unread.is_some() { get_i64(row, pos(4)) } else { 0 };
        let type_raw = if c_type.is_some() { get_i64(row, pos(5)) } else { 0 };
        let kind = match type_raw {
            2 => SessionKind::Group,
            3 => SessionKind::Official,
            _ => SessionKind::classify(&username),
        };
        store.sessions.insert(
            username.clone(),
            Session {
                username,
                display_name,
                kind,
                last_timestamp,
                last_msg_type,
                summary,
                unread_count,
                message_count: 0,
            },
        );
    }
    Ok(())
}

/// Parse the `contact` table of `contact.db`.
pub fn load_contacts(conn: &Connection, store: &mut Store) -> Result<()> {
    let cols = open::table_columns(conn, "contact");
    let c_username = pick(&cols, CONTACT_COLUMNS[0]);
    let c_remark = pick(&cols, CONTACT_COLUMNS[1]);
    let c_nick = pick(&cols, CONTACT_COLUMNS[2]);
    let c_alias = pick(&cols, CONTACT_COLUMNS[3]);
    let c_avatar = pick(&cols, CONTACT_COLUMNS[4]);
    let c_type = pick(&cols, CONTACT_COLUMNS[5]);
    let Some(c_username) = c_username else {
        return Ok(());
    };
    let selected: Vec<&str> = [
        Some(c_username),
        c_remark,
        c_nick,
        c_alias,
        c_avatar,
        c_type,
    ]
    .into_iter()
    .flatten()
    .collect();
    let query = format!("SELECT {} FROM contact", selected.join(", "));
    let mut stmt = conn.prepare(&query)?;
    let has = [c_remark.is_some(), c_nick.is_some(), c_alias.is_some(), c_avatar.is_some(), c_type.is_some()];
    let pos = |n: usize| -> usize { 1 + has[..n].iter().filter(|b| **b).count() };
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let username = get_opt_str(row, 0).unwrap_or_default();
        if username.is_empty() {
            continue;
        }
        let remark = if c_remark.is_some() { get_opt_str(row, pos(0)).filter(|s| !s.is_empty()) } else { None };
        let nickname = if c_nick.is_some() { get_opt_str(row, pos(1)).filter(|s| !s.is_empty()) } else { None };
        let alias = if c_alias.is_some() { get_opt_str(row, pos(2)).filter(|s| !s.is_empty()) } else { None };
        let avatar_url = if c_avatar.is_some() { get_opt_str(row, pos(3)).filter(|s| !s.is_empty()) } else { None };
        let type_raw = if c_type.is_some() { get_i64(row, pos(4)) } else { 0 };
        let kind = match type_raw {
            1 => SessionKind::Private,
            2 => SessionKind::Group,
            3 => SessionKind::Official,
            _ => SessionKind::classify(&username),
        };
        store.contacts.insert(
            username.clone(),
            Contact {
                username,
                remark,
                nickname,
                alias,
                avatar_url,
                kind,
            },
        );
    }
    Ok(())
}

/// Find message tables (`Msg_<md5>` / `msg_<md5>`); returns (name, md5 suffix).
pub fn message_tables(conn: &Connection) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for t in open::table_names(conn, 1000) {
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("msg_")
            && (rest.len() == 32 || rest.len() == 16)
                && rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                    out.push((t, rest.to_string()));
                }
    }
    out
}

/// Find the `Name2Id%` lookup table (sender rowid -> user_name).
pub fn name2id_table(conn: &Connection) -> Option<String> {
    let mut names = open::table_names(conn, 500);
    names.sort();
    names.into_iter().rev().find(|t| t.to_ascii_lowercase().starts_with("name2id"))
}

/// Resolve the session username a message table belongs to: md5 tables are
/// keyed by md5(username); if the hash matches a known session we recover
/// the username, else the hash itself becomes the conversation key.
fn table_session(md5_suffix: &str, store: &Store) -> String {
    use md5::Digest;
    let suffix_lower = md5_suffix.to_lowercase();
    for username in store.sessions.keys() {
        let mut h = md5::Md5::new();
        h.update(username.as_bytes());
        let hex = format!("{:x}", h.finalize());
        if hex.starts_with(&suffix_lower) {
            return username.clone();
        }
    }
    md5_suffix.to_string()
}

/// Public wrapper for table-name -> session resolution (used by sync).
pub fn resolve_table_session(store: &Store, md5_suffix: &str) -> String {
    table_session(md5_suffix, store)
}

/// Row projection shared by the full build and incremental reads.
struct MsgCols {
    local: String,
    server: Option<String>,
    kind: Option<String>,
    time: Option<String>,
    seq: Option<String>,
    sender: Option<String>,
    real: Option<String>,
    content: Option<String>,
    compress: Option<String>,
    /// selected column names in SELECT order
    selected: Vec<String>,
    /// positions of each optional column inside a row
    pos_server: usize,
    pos_kind: usize,
    pos_time: usize,
    pos_seq: usize,
    pos_sender: usize,
    pos_real: usize,
    pos_content: usize,
    pos_compress: usize,
}

impl MsgCols {
    fn probe(conn: &Connection, table: &str) -> Option<MsgCols> {
        let cols = open::table_columns(conn, table);
        let local = pick(&cols, MSG_COLUMNS[0])?.to_string();
        let server = pick(&cols, MSG_COLUMNS[1]).map(str::to_string);
        let kind = pick(&cols, MSG_COLUMNS[2]).map(str::to_string);
        let time = pick(&cols, MSG_COLUMNS[3]).map(str::to_string);
        let seq = pick(&cols, MSG_COLUMNS[4]).map(str::to_string);
        let sender = pick(&cols, MSG_COLUMNS[5]).map(str::to_string);
        let real = pick(&cols, MSG_COLUMNS[6]).map(str::to_string);
        let content = cols
            .iter()
            .find(|c| c.eq_ignore_ascii_case("message_content"))
            .cloned();
        let compress = cols
            .iter()
            .find(|c| c.eq_ignore_ascii_case("compress_content"))
            .cloned();
        let mut selected = vec![local.clone()];
        for c in [&server, &kind, &time, &seq, &sender, &real, &content, &compress]
            .into_iter()
            .flatten()
        {
            if !selected.iter().any(|s| s == c) {
                selected.push(c.clone());
            }
        }
        let pos = |name: &str| selected.iter().position(|s| s == name).unwrap();
        let pos_server = server.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_kind = kind.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_time = time.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_seq = seq.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_sender = sender.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_real = real.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_content = content.as_ref().map(|c| pos(c)).unwrap_or(0);
        let pos_compress = compress.as_ref().map(|c| pos(c)).unwrap_or(0);
        Some(MsgCols {
            local,
            server,
            kind,
            time,
            seq,
            sender,
            real,
            content,
            compress,
            selected,
            pos_server,
            pos_kind,
            pos_time,
            pos_seq,
            pos_sender,
            pos_real,
            pos_content,
            pos_compress,
        })
    }

    /// Extract one owned message row from a rusqlite row.
    fn extract(&self, row: &Row, name2id: &std::collections::HashMap<i64, String>) -> OwnedMsgRow {
        let local_id = get_i64(row, 0);
        let server_id = if self.server.is_some() { get_i64(row, self.pos_server) } else { 0 };
        let local_type = if self.kind.is_some() { get_i64(row, self.pos_kind) } else { 0 };
        let create_time = if self.time.is_some() { get_i64(row, self.pos_time) } else { 0 };
        let sort_seq = if self.seq.is_some() { get_i64(row, self.pos_seq) } else { 0 };
        let sender_username = if self.sender.is_some() {
            get_opt_str(row, self.pos_sender).filter(|s| !s.is_empty()).unwrap_or_default()
        } else {
            String::new()
        };
        let sender_username = if sender_username.is_empty() && self.real.is_some() {
            let rid = get_i64(row, self.pos_real);
            name2id.get(&rid).cloned().unwrap_or_default()
        } else {
            sender_username
        };
        let content = if self.content.is_some() {
            // raw bytes: message_content may be a zstd frame (WeChat 4.1.x)
            get_opt_bytes(row, self.pos_content)
        } else {
            None
        };
        let compress = if self.compress.is_some() {
            get_opt_blob(row, self.pos_compress)
        } else {
            None
        };
        OwnedMsgRow {
            local_id,
            server_id,
            local_type,
            create_time,
            sort_seq,
            sender_username,
            content,
            compress,
        }
    }
}

struct OwnedMsgRow {
    local_id: i64,
    server_id: i64,
    local_type: i64,
    create_time: i64,
    sort_seq: i64,
    sender_username: String,
    /// `message_content` raw bytes (possibly a zstd frame)
    content: Option<Vec<u8>>,
    compress: Option<Vec<u8>>,
}

fn row_to_record(row: OwnedMsgRow, my_wxid: &str, contacts: &std::collections::HashMap<String, Contact>) -> MessageRecord {
    let content_str = parser::decode_content(row.compress.as_deref(), row.content.as_deref())
        .unwrap_or_else(|| {
            String::from_utf8_lossy(row.content.as_deref().unwrap_or_default()).into_owned()
        });
    let parsed = parser::parse_message(row.local_type, row.server_id, row.local_id, &content_str);
    let is_send = !row.sender_username.is_empty() && row.sender_username == my_wxid;
    let sender_name = contacts
        .get(&row.sender_username)
        .map(|c| c.display_name())
        .filter(|s| !s.is_empty() && s != &row.sender_username)
        .unwrap_or_else(|| row.sender_username.clone());
    MessageRecord {
        local_id: row.local_id,
        server_id: row.server_id,
        local_type: row.local_type,
        create_time: row.create_time,
        sort_seq: row.sort_seq,
        is_send,
        sender_username: row.sender_username,
        sender_name,
        parsed,
    }
}

/// Load the moments timeline (`sns.db:SnsTimeLine`), newest first.
pub fn load_sns(conn: &Connection, store: &mut Store) -> Result<()> {
    let mut feeds = Vec::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT tid, user_name, content FROM SnsTimeLine")
    else {
        return Ok(());
    };
    let rows: Vec<(i64, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0).unwrap_or(0),
                r.get::<_, String>(1).unwrap_or_default(),
                r.get::<_, String>(2).unwrap_or_default(),
            ))
        })?
        .flatten()
        .collect();
    for (tid, user_name, content) in rows {
        let feed = parser::parse_sns_feed(&user_name, &tid.to_string(), &content);
        feeds.push(feed);
    }
    feeds.sort_by(|a, b| b.create_time.cmp(&a.create_time).then(b.feed_id.cmp(&a.feed_id)));
    store.sns_feeds = feeds;
    Ok(())
}

/// Load the `Name2Id` uid map: rowid -> user_name.
fn load_uid_map(conn: &Connection, name2id: Option<&str>) -> std::collections::HashMap<i64, String> {
    let mut map = std::collections::HashMap::new();
    let Some(t) = name2id else { return map };
    let Ok(mut stmt) = conn.prepare(&format!("SELECT rowid, user_name FROM \"{t}\"")) else {
        return map;
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return map,
    };
    while let Ok(Some(row)) = rows.next() {
        if let (Ok(id), Ok(name)) = (row.get::<_, i64>(0), row.get::<_, String>(1)) {
            map.insert(id, name);
        }
    }
    map
}

/// Read all rows of one message table (in order), returning owned records and
/// the table watermark. Sender names are resolved via `contacts`.
fn load_message_table(
    conn: &Connection,
    table: &str,
    uid_map: &std::collections::HashMap<i64, String>,
    contacts: &std::collections::HashMap<String, Contact>,
    my_wxid: &str,
) -> Result<(Vec<MessageRecord>, Watermark)> {
    let cols = MsgCols::probe(conn, table).ok_or_else(|| anyhow::anyhow!("no local_id column"))?;
    let order = [
        cols.time.as_deref().unwrap_or(&cols.local),
        cols.seq.as_deref().unwrap_or(&cols.local),
        &cols.local,
    ];
    let query = format!(
        "SELECT {sel} FROM \"{table}\" ORDER BY {ord}",
        sel = cols.selected.join(", "),
        ord = order.join(", "),
    );
    let mut stmt = conn.prepare(&query)?;
    let mut rows = stmt.query([])?;
    let mut records = Vec::new();
    let mut watermark = Watermark::default();
    while let Some(row) = rows.next()? {
        let owned = cols.extract(row, uid_map);
        watermark = max_watermark(
            watermark,
            Watermark {
                create_time: owned.create_time,
                sort_seq: owned.sort_seq,
                local_id: owned.local_id,
            },
        );
        records.push(row_to_record(owned, my_wxid, contacts));
    }
    Ok((records, watermark))
}

fn max_watermark(a: Watermark, b: Watermark) -> Watermark {
    if (b.create_time, b.sort_seq, b.local_id) > (a.create_time, a.sort_seq, a.local_id) {
        b
    } else {
        a
    }
}

/// Build the full index for one account by reading its live databases
/// directly through the pooled read-only connections.
pub fn build_all_live(
    pool: &mut LivePool,
    keys: &KeyMap,
    my_wxid: &str,
    db_files: &[DbFile],
) -> Result<Store> {
    let mut store = Store {
        my_wxid: my_wxid.to_string(),
        ..Default::default()
    };

    // 1) session.db
    if let Some(f) = db_files.iter().find(|f| f.kind == DbKind::Session) {
        let Some(key) = keys.key_for(&f.rel) else {
            tracing::warn!("no key for session.db");
            return Ok(store);
        };
        match pool.get_or_open(f, key) {
            Ok(conn) => {
                if let Err(e) = load_sessions(conn, &mut store) {
                    tracing::warn!("session table unreadable: {e}");
                }
            }
            Err(e) => tracing::warn!("session.db live open failed: {e}"),
        }
    }

    // 2) contact.db
    if let Some(f) = db_files.iter().find(|f| f.kind == DbKind::Contact) {
        let Some(key) = keys.key_for(&f.rel) else {
            tracing::warn!("no key for contact.db");
            return Ok(store);
        };
        match pool.get_or_open(f, key) {
            Ok(conn) => {
                if let Err(e) = load_contacts(conn, &mut store) {
                    tracing::warn!("contact table unreadable: {e}");
                }
            }
            Err(e) => tracing::warn!("contact.db live open failed: {e}"),
        }
    }

    // 3) message databases (contacts snapshot is immutable during build)
    let contacts = store.contacts.clone();
    for f in db_files.iter().filter(|f| f.kind == DbKind::Message) {
        let Some(key) = keys.key_for(&f.rel) else {
            tracing::warn!("no key for {}", f.rel);
            continue;
        };
        let conn = match pool.get_or_open(f, key) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("{} live open failed: {e}", f.rel);
                continue;
            }
        };
        let uid_map = load_uid_map(conn, name2id_table(conn).as_deref());
        for (table, md5_suffix) in message_tables(&conn) {
            match load_message_table(conn, &table, &uid_map, &contacts, my_wxid) {
                Ok((records, watermark)) => {
                    let session = table_session(&md5_suffix, &store);
                    let conv = store.convs.entry(session).or_default();
                    conv.extend(records);
                    let current = store
                        .watermarks
                        .get(&format!("{}:{table}", f.rel))
                        .copied()
                        .unwrap_or_default();
                    let wm = max_watermark(current, watermark);
                    store.watermarks.insert(format!("{}:{table}", f.rel), wm);
                }
                Err(e) => {
                    tracing::warn!("message table {table} skipped: {e}");
                }
            }
        }
    }

    // 4) sns.db — moments timeline
    if let Some(f) = db_files.iter().find(|f| f.kind == DbKind::Sns) {
        let Some(key) = keys.key_for(&f.rel) else {
            tracing::warn!("no key for {}", f.rel);
            return Ok(store);
        };
        match pool.get_or_open(f, key) {
            Ok(conn) => {
                if let Err(e) = load_sns(conn, &mut store) {
                    tracing::warn!("sns timeline unreadable: {e}");
                }
            }
            Err(e) => tracing::warn!("{} live open failed: {e}", f.rel),
        }
    }

    // 5) fill session message counts from convs
    for (username, s) in store.sessions.iter_mut() {
        s.message_count = store.convs.get(username).map_or(0, |v| v.len());
    }
    Ok(store)
}

/// Incremental read: rows of one table after the watermark, in order.
pub fn read_new(
    conn: &Connection,
    table: &str,
    watermark: &Watermark,
    name2id: Option<&str>,
) -> Result<Vec<MessageRecord>> {
    let cols =
        MsgCols::probe(conn, table).ok_or_else(|| anyhow::anyhow!("no local_id column"))?;
    let Some(time) = cols.time.as_deref() else {
        return Ok(Vec::new()); // no time column: cannot do window reads
    };
    let uid_map = load_uid_map(conn, name2id);
    let order = [time, cols.seq.as_deref().unwrap_or(&cols.local), &cols.local];
    let query = format!(
        "SELECT {sel} FROM \"{table}\" WHERE ({ts}, {seq}, {lid}) > (?1, ?2, ?3) ORDER BY {ord} LIMIT 5000",
        sel = cols.selected.join(", "),
        ord = order.join(", "),
        ts = time,
        seq = cols.seq.as_deref().unwrap_or(&cols.local),
        lid = cols.local,
    );
    let mut stmt = conn.prepare(&query)?;
    let mut rows = stmt.query(rusqlite::params![
        watermark.create_time,
        watermark.sort_seq,
        watermark.local_id
    ])?;
    let mut out = Vec::new();
    // sender display resolution needs contacts, which we do not have here;
    // caller (sync engine) re-resolves names after applying
    let empty: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let no_contacts: std::collections::HashMap<String, Contact> = std::collections::HashMap::new();
    while let Some(row) = rows.next()? {
        let owned = cols.extract(row, &uid_map);
        out.push(row_to_record(owned, "", &no_contacts));
    }
    let _ = empty;
    Ok(out)
}
