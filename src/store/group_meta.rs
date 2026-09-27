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

use rusqlite::Connection;

use crate::store::{GroupCards, Store};

/// 从 `contact.db` 读**群主**与**名册**。
///
/// 两者都用 `contact.db` **自己**的 `name2id` 解析 `room_id`/`member_id`/`owner`。
/// 表或列缺失时静默跳过 —— 老版本微信可能没有这些表，**降级不是错误**。
pub fn load_chatroom_meta(conn: &Connection, store: &mut Store) {
    let name2id = super::index::uid_map(conn);
    if name2id.is_empty() {
        return;
    }

    // 群主：`chat_room`。**这一张表用的是用户名，不是 rowid** —— 真库实测它的列是
    // (id, username, owner, ext_buffer)：`username` 是群、`owner` 是群主，两者都已经是
    // 用户名；按 rowid 去 name2id 里查会**一个都查不到**（实测 0/114，而按用户名匹配 114/114）。
    // 同一个库里 `chatroom_member` 用的却**是** rowid —— 两张表形态不同，不能一起假设。
    if let Ok(mut stmt) = conn.prepare("SELECT username, owner FROM chat_room") {
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
        });
        if let Ok(rows) = rows {
            for (room, owner) in rows.filter_map(Result::ok) {
                let (Some(room), Some(owner)) = (room, owner) else {
                    continue;
                };
                if !room.is_empty() && !owner.is_empty() {
                    store.chatroom_owner.insert(room, owner);
                }
            }
        }
    }

    // 名册：`chatroom_member(room_id, member_id)`，逐房间去重。
    if let Ok(mut stmt) = conn.prepare("SELECT room_id, member_id FROM chatroom_member")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
    {
        {
            for (room_id, member_id) in rows.filter_map(Result::ok) {
                let (Some(room), Some(member)) = (name2id.get(&room_id), name2id.get(&member_id))
                else {
                    continue;
                };
                let roster = store.chatroom_roster.entry(room.clone()).or_default();
                if !roster.contains(member) {
                    roster.push(member.clone());
                }
            }
        }
    }
}

/// 从 `contact_fts.db` 读**群名片**。
///
/// 读的是影子表：`_aux(room_id, member_id)` 与 `_content(id, c0, c1, c2)` 按 `rowid` 连接
/// （实测 6029/6029 = 100%）。`c0` 是 FTS 表的第一个列 —— 建表语句里就是 `a_group_remark`，
/// 即群名片。**`c1`/`c2` 是 `room_id`/`member_id` 的副本，但以 `_aux` 为准**：影子表的列顺序
/// 是实现细节，而 `_aux` 的列名是稳定的。
///
/// id 用**本库自己的** `name2id` 解析 —— 用 `contact.db` 的会解析到错误的人。
pub fn load_group_cards(conn: &Connection, store: &mut Store) {
    let name2id = super::index::uid_map(conn);
    if name2id.is_empty() {
        return;
    }
    let mut stmt = match conn.prepare(
        "SELECT a.room_id, a.member_id, c.c0 \
           FROM chatroom_member_fts_v3_aux a \
           JOIN chatroom_member_fts_v3_content c ON c.rowid = a.rowid",
    ) {
        Ok(s) => s,
        // 影子表不在（老版本、或该库没有这张 FTS 表）不是错误。
        Err(_) => return,
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    }) else {
        return;
    };
    let mut cards: GroupCards = HashMap::new();
    for (room_id, member_id, card) in rows.filter_map(Result::ok) {
        let Some(card) = card.filter(|c| !c.is_empty()) else {
            continue;
        };
        let (Some(room), Some(member)) = (name2id.get(&room_id), name2id.get(&member_id)) else {
            continue;
        };
        cards
            .entry(room.clone())
            .or_default()
            .insert(member.clone(), card);
    }
    if !cards.is_empty() {
        store.group_cards = cards;
    }
}
