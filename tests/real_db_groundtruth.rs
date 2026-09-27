//! Ground-truth probes against a REAL WeChat 4.0 account.
//!
//! Skipped by default. Enable with:
//!   WEFLOW_TEST_DB_ROOT   = account directory (contains db_storage/)
//!   WEFLOW_TEST_KEYS_JSON = all_keys.json style {rel: {enc_key}} map
//!   (or WEFLOW_TEST_KEY for a uniform key)
//!
//! These never print message contents or third-party identifiers.

mod common;

use std::env;
use std::fs;
use std::path::PathBuf;

use weflow_server::db::live::LivePool;
use weflow_server::db::{scan, wcdb};
use weflow_server::keystore;
use weflow_server::store::index;

/// Load the real-db key source: either a single key (`WEFLOW_TEST_KEY`) or a
/// per-database key map (`WEFLOW_TEST_KEYS_JSON`, the `all_keys.json` format).
fn real_env() -> Option<(PathBuf, keystore::KeyMap)> {
    let root = PathBuf::from(env::var("WEFLOW_TEST_DB_ROOT").ok()?);
    if let Ok(json_path) = env::var("WEFLOW_TEST_KEYS_JSON") {
        let text = fs::read_to_string(json_path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let mut map = std::collections::HashMap::new();
        if let Some(obj) = v.as_object() {
            for (rel, entry) in obj {
                if rel.starts_with('_') {
                    continue;
                }
                let hex = entry.get("enc_key")?.as_str()?.to_string();
                let k = keystore::parse_db_key(&hex).ok()?;
                map.insert(rel.replace('\\', "/"), k);
            }
        }
        Some((root, keystore::KeyMap::Map(map)))
    } else {
        let key_hex = env::var("WEFLOW_TEST_KEY").ok()?;
        let key = keystore::parse_db_key(&key_hex).ok()?;
        Some((root, keystore::KeyMap::Single(key)))
    }
}

#[test]
#[ignore = "requires a real WeChat 4.0 account"]
fn real_session_db_roundtrips() {
    let Some((root, keys)) = real_env() else { return };
    let session = root.join("db_storage/session/session.db");
    assert!(session.is_file(), "no session.db at {}", session.display());
    let enc = fs::read(&session).unwrap();
    let key = keys.key_for("session/session.db").expect("session key");
    let plain = wcdb::decrypt_db(&key.0, &enc).expect("decrypt");
    common::assert_wechat_layout(&plain);
}

#[test]
#[ignore = "requires a real WeChat 4.0 account"]
fn real_account_indexes() {
    let Some((root, keys)) = real_env() else { return };
    let storage = root.join("db_storage");
    let files = scan::enum_db_files(&storage);
    assert!(!files.is_empty());
    let mut pool = LivePool::new();
    let store = index::build_all_live(&mut pool, &keys, "real", &files).unwrap();
    eprintln!("index ok");
    assert!(
        !store.sessions.is_empty() || !store.convs.is_empty(),
        "expected indexed data"
    );
}

/// Full-database probe (privacy-safe: counts and schema shapes only).
#[test]
#[ignore = "requires a real WeChat 4.0 account"]
fn real_db_full_probe() {
    let Some((root, keys)) = real_env() else { return };
    let storage = root.join("db_storage");
    let files = scan::enum_db_files(&storage);

    eprintln!("== decrypt probe: {} db files ==", files.len());
    let mut matched = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for f in &files {
        let enc = match fs::read(&f.abs) {
            Ok(e) => e,
            Err(e) => { failed.push((f.rel.clone(), format!("read: {e}"))); continue; }
        };
        let Some(key) = keys.key_for(&f.rel) else {
            failed.push((f.rel.clone(), "no key entry".into()));
            continue;
        };
        if !wcdb::verify_page1(&key.0, &enc[..enc.len().min(4096)]) {
            failed.push((f.rel.clone(), "page-1 HMAC mismatch".into()));
            continue;
        }
        matched += 1;
        let plain = wcdb::decrypt_db(&key.0, &enc).expect("decrypt after verify");
        common::assert_wechat_layout(&plain);
    }
    eprintln!("HMAC matched: {matched}/{}", files.len());
    for (rel, e) in &failed { eprintln!("  FAIL {rel}: {e}"); }

    // live-pool index build
    let mut pool = LivePool::new();
    let store = index::build_all_live(&mut pool, &keys, "real", &files).unwrap();
    eprintln!(
        "sessions={} convs={} contacts={}",
        store.sessions.len(),
        store.convs.len(),
        store.contacts.len()
    );
    let total_msgs: usize = store.convs.values().map(|v| v.len()).sum();
    eprintln!("messages={total_msgs}");
    assert!(!store.convs.is_empty(), "expected conversations");
}

/// 探查 `contact_fts.db` 的**表结构**：群昵称的来源与可达性。
///
/// 为什么单独探它：`chatroom_member_fts_v3` 是带**自定义分词器**（`MMFtsTokenizer`）的 FTS5
/// 虚拟表 —— 普通 SQLite 建不出来，因此**夹具无法造它**。若生产代码只读虚拟表，夹具路径就
/// 永远验证不了群昵称。影子表（`_content` / `_aux`）是普通表，能不能只靠它们取到同样的数据，
/// 决定了「夹具能不能造」。
///
/// 只输出**表名、列名、行数与可达性计数**，不打印任何一行的内容。
#[tokio::test]
#[ignore = "requires a real WeChat 4.0 account"]
async fn real_contact_fts_schema_probe() {
    use rusqlite::Connection;
    let Some((root, keys)) = real_env() else {
        println!("[probe] 未提供真库环境，跳过");
        return;
    };
    let path = root.join("db_storage/contact/contact_fts.db");
    let Some(key) = keys.key_for("contact/contact_fts.db") else {
        println!("[probe] 密钥表里没有 contact/contact_fts.db 的条目");
        return;
    };
    let conn = Connection::open(&path).expect("打开 contact_fts.db");
    // 与 `db/live.rs` 同形的原始密钥写法。
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex::encode(key.0)))
        .expect("上密钥");

    // 1. 所有表/虚拟表的名字与建表语句（只输出结构）。
    let mut stmt = conn
        .prepare("SELECT type, name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")
        .unwrap();
    let objs: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    println!("[probe] contact_fts.db 的对象数 = {}", objs.len());
    for (t, n) in &objs {
        // 只对名字里含 fts 的对象打印（其余是与本任务无关的表）。
        if n.contains("fts") {
            println!("[probe]   {t} {n}");
        }
    }

    // 2. 每个 fts 相关对象的列名与行数。
    for (_, name) in objs.iter().filter(|(_, n)| n.contains("fts")) {
        let cols: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info(\"{name}\")"))
            .and_then(|mut s| {
                Ok(s.query_map([], |r| r.get::<_, String>(1))?
                    .filter_map(Result::ok)
                    .collect::<Vec<_>>())
            })
            .unwrap_or_default();
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |r| r.get(0))
            .unwrap_or(-1);
        println!("[probe]   {name}: {} 列, {n} 行, 列名 = {:?}", cols.len(), cols);
    }

    // 3. 关键问题：影子表能不能独立给出 (room, member, card)？
    let aux = "chatroom_member_fts_v3_aux";
    let content = "chatroom_member_fts_v3_content";
    let has_aux = objs.iter().any(|(_, n)| n == aux);
    let has_content = objs.iter().any(|(_, n)| n == content);
    println!("[probe] 影子表存在性: _aux={has_aux} _content={has_content}");
    if has_aux && has_content {
        let joined: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM \"{aux}\" a JOIN \"{content}\" c ON c.rowid = a.rowid"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        println!("[probe] _aux JOIN _content ON rowid 的配对数 = {joined}");

        // 4. 关键验证：**只靠影子表**能不能拿到 (room, member, 群昵称)？
        //    若可以，夹具就能造它（普通表），生产代码也就能不依赖自定义分词器。
        let total: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{content}\""), [], |r| r.get(0))
            .unwrap_or(-1);
        let nonempty: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{content}\" WHERE c0 IS NOT NULL AND c0 <> ''"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        let rooms: i64 = conn
            .query_row(
                &format!("SELECT COUNT(DISTINCT room_id) FROM \"{aux}\""),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        let members: i64 = conn
            .query_row(
                &format!("SELECT COUNT(DISTINCT member_id) FROM \"{aux}\""),
                [],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        println!("[probe] 只靠影子表：总 {total} 行，非空群昵称 {nonempty} 行，{rooms} 个房间，{members} 个成员");

        // 5. **实现的关键坑**：`room_id`/`member_id` 不在 contact.db 的 id 空间里，
        //    必须用 contact_fts.db **自己**的 `name2id`。验证它在这里确实存在、且能解析。
        let has_name2id = objs.iter().any(|(_, n)| n == "name2id");
        println!("[probe] contact_fts.db 里有 name2id: {has_name2id}");
        if has_name2id {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM name2id", [], |r| r.get(0))
                .unwrap_or(-1);
            // 解析率：_aux 的 room_id / member_id 有多少能在本库的 name2id 里找到。
            let room_hit: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM \"{aux}\" a JOIN name2id n ON n.rowid = a.room_id"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            let member_hit: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM \"{aux}\" a JOIN name2id n ON n.rowid = a.member_id"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            // 解析出来的名字里有多少长得像群（`@chatroom`）—— 只看形态，不打印名字。
            let room_like: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM \"{aux}\" a JOIN name2id n ON n.rowid = a.room_id \
                         WHERE n.username LIKE '%@chatroom'"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            println!("[probe] name2id 有 {n} 行；room 解析 {room_hit}（其中 @chatroom 形态 {room_like}），member 解析 {member_hit}");
        }
    }
}