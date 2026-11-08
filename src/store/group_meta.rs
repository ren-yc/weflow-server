//! 群元数据（群名片 / 群主 / 名册）。
//!
//! ## 为什么单独一个模块
//!
//! 这三样东西散在**两个库、三张表**里，而且两个库的 **id 空间互相独立** —— 这是最容易写错的
//! 地方：`contact_fts.db` 里的 `room_id`/`member_id` **不能**用 `contact.db` 的 `name2id` 解析
//! （实测 4889 行 vs 4933 行，同一 username 的 rowid 不同）。每个库必须用**它自己**的 `name2id`。
//!
//! | 数据 | 来源 |
//! |---|---|
//! | 群名片 | `contact_fts.db` 的 FTS 影子表 `chatroom_member_fts_v3_content` / `_aux` |
//! | 群主 | `contact.db::chat_room.owner` |
//! | 名册 | `contact.db::chatroom_member` |
//!
//! ## 为什么读影子表而不是那张 FTS 虚拟表
//!
//! `chatroom_member_fts_v3` 是**带 WeChat 自定义分词器**（`MMFtsTokenizer`）的 FTS5 虚拟表 ——
//! 普通 SQLite **建不出来**，于是**夹具无法造它**，群名片就永远无法在夹具路径验证（这正是这个
//! 缺陷能藏这么久的原因）。影子表 `_content` / `_aux` 是普通表，实测与虚拟表数据一致
//! （6029 行、114 个房间），夹具用两张普通表就能造出等价数据。

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;

use crate::store::{Contact, GroupCards, Store};

/// A complete contact.db snapshot. Applying it replaces deleted entries too.
pub struct ContactMetadata {
    contacts: HashMap<String, Contact>,
    owners: HashMap<String, String>,
    rosters: HashMap<String, Vec<String>>,
}

impl ContactMetadata {
    pub fn apply(self, store: &mut Store) {
        store.contacts = self.contacts;
        store.chatroom_owner = self.owners;
        store.chatroom_roster = self.rosters;
    }
}

/// Read contacts, owners and members in one consistent read view. No shared
/// Store is changed until every query and row decode has succeeded.
pub fn read_contact_metadata(conn: &Connection) -> Result<ContactMetadata> {
    let tx = conn.unchecked_transaction()?;
    let mut staged = Store::default();
    super::index::load_contacts(&tx, &mut staged)?;
    load_chatroom_meta(&tx, &mut staged)?;
    tx.commit()?;
    Ok(ContactMetadata {
        contacts: staged.contacts,
        owners: staged.chatroom_owner,
        rosters: staged.chatroom_roster,
    })
}

/// Only proven missing tables/columns are optional. Busy, I/O and schema
/// query errors must propagate rather than masquerading as empty metadata.
fn has_columns(conn: &Connection, table: &str, required: &[&str]) -> Result<bool> {
    let columns = crate::db::open::table_columns_checked(conn, table)?;
    Ok(required.iter().all(|name| columns.iter().any(|c| c.eq_ignore_ascii_case(name))))
}

/// Read contact.db owners and roster, using this database's own Name2Id map.
/// Both maps replace their previous snapshots only on a complete read.
pub fn load_chatroom_meta(conn: &Connection, store: &mut Store) -> Result<()> {
    let mut owners = HashMap::new();
    let mut rosters: HashMap<String, Vec<String>> = HashMap::new();
    // Owners are usernames, independent of the numeric membership mapping.
    if has_columns(conn, "chat_room", &["username", "owner"])? {
        let mut stmt = conn.prepare("SELECT username, owner FROM chat_room")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (Some(room), Some(owner)) = row? else { continue };
            if !room.is_empty() && !owner.is_empty() {
                owners.insert(room, owner);
            }
        }
    }
    if has_columns(conn, "chatroom_member", &["room_id", "member_id"])? {
        let names = super::index::uid_map(conn)?;
        let mut stmt = conn.prepare("SELECT room_id, member_id FROM chatroom_member")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (room_id, member_id) = row?;
            let (Some(room), Some(member)) = (names.get(&room_id), names.get(&member_id)) else {
                continue;
            };
            let roster = rosters.entry(room.clone()).or_default();
            if !roster.contains(member) {
                roster.push(member.clone());
            }
        }
    }
    store.chatroom_owner = owners;
    store.chatroom_roster = rosters;
    Ok(())
}

/// Read FTS shadow tables in a consistent view with their own Name2Id map.
/// An empty snapshot clears removed cards; query/iteration errors retain the
/// old snapshot because the caller cannot apply an incomplete result.
pub fn read_group_cards(conn: &Connection) -> Result<GroupCards> {
    let tx = conn.unchecked_transaction()?;
    let mut cards = HashMap::<String, HashMap<String, String>>::new();
    if has_columns(&tx, "chatroom_member_fts_v3_aux", &["room_id", "member_id"])?
        && has_columns(&tx, "chatroom_member_fts_v3_content", &["c0"])?
    {
        let names = super::index::uid_map(&tx)?;
        let mut stmt = tx.prepare(
            "SELECT a.room_id, a.member_id, c.c0 FROM chatroom_member_fts_v3_aux a \
             JOIN chatroom_member_fts_v3_content c ON c.rowid = a.rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))
        })?;
        for row in rows {
            let (room_id, member_id, card) = row?;
            let Some(card) = card.filter(|c| !c.is_empty()) else { continue };
            let (Some(room), Some(member)) = (names.get(&room_id), names.get(&member_id)) else {
                continue;
            };
            cards.entry(room.clone()).or_default().insert(member.clone(), card);
        }
    }
    tx.commit()?;
    Ok(cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_schema_is_empty_but_query_and_decode_failures_are_errors() {
        let conn = Connection::open_in_memory().unwrap();
        let mut store = Store::default();
        read_contact_metadata(&conn).unwrap().apply(&mut store);
        assert!(store.contacts.is_empty());
        assert!(read_group_cards(&conn).unwrap().is_empty());
        // Owners do not depend on the optional numeric name map.
        conn.execute_batch(
            "CREATE TABLE contact (username TEXT, remark TEXT);
             INSERT INTO contact VALUES ('member', 'before');
             CREATE TABLE chat_room (username TEXT, owner TEXT);
             INSERT INTO chat_room VALUES ('room', 'member');
             CREATE TABLE chatroom_member (room_id INTEGER);"
        ).unwrap();
        read_contact_metadata(&conn).unwrap().apply(&mut store);
        assert_eq!(store.chatroom_owner["room"], "member");
        assert!(store.chatroom_roster.is_empty());
        conn.execute_batch("INSERT INTO contact VALUES ('invalid', x'80');").unwrap();
        assert!(read_contact_metadata(&conn).is_err(), "partial contact decode is a failure");
        assert_eq!(store.contacts.len(), 1);
        conn.execute_batch(
            "DROP TABLE contact; CREATE VIEW contact AS SELECT missing FROM no_such_table;"
        ).unwrap();
        assert!(read_contact_metadata(&conn).is_err(), "schema query errors are not absence");
    }

    #[test]
    fn metadata_lock_error_is_not_an_absent_optional_table() {
        let dir = std::env::temp_dir().join(format!("weflow_metadata_lock_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("contact.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE IF NOT EXISTS contact (username TEXT);").unwrap();
        let reader = crate::db::open::open_snapshot(&path).unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        assert!(read_contact_metadata(&reader).is_err());
        assert!(read_group_cards(&reader).is_err());
        writer.execute_batch("ROLLBACK;").unwrap();
        assert!(read_contact_metadata(&reader).is_ok());
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
