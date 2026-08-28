//! Shared test fixtures: build *real* SQLCipher-encrypted databases (WeChat
//! 4.0-compatible: AES-256-CBC, HMAC-SHA512, reserve=80, 4096B pages, salt in
//! page 1) using the bundled rusqlite-sqlcipher, then feed them through
//! `db::wcdb::decrypt_db` — the ground-truth interop arbitration.

use std::fs;
use std::path::{Path, PathBuf};

use rand::RngCore;
use rusqlite::Connection;
use weflow_server::db::wcdb::{self, Key};

pub const FAKE_KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
pub const FAKE_WXID: &str = "wxid_fake000000000000001";
pub const FAKE_FRIEND: &str = "wxid_friend_a";
pub const FAKE_GROUP: &str = "wxid_fake_group@chatroom";

/// Unique temp dir per test binary invocation (kept under target/).
pub fn tmp_dir(tag: &str) -> PathBuf {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp");
    fs::create_dir_all(&base).unwrap();
    let dir = base.join(format!("{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Open (or create) a SQLCipher-encrypted database with WeChat-4-compatible
/// parameters. Pragma order matters: page size before key; journal mode after.
pub fn wx_conn(path: &Path, key: &Key, wal: bool) -> Connection {
    let conn = Connection::open(path).unwrap();
    let key_hex = hex::encode(key);
    conn.execute_batch(&format!(
        "PRAGMA cipher_page_size = 4096;
         PRAGMA key = \"x'{key_hex}'\";
         PRAGMA journal_mode = {};",
        if wal { "WAL" } else { "DELETE" }
    ))
    .unwrap();
    conn
}

/// Build a WeChat-4-like encrypted account (db_storage layout) under `dir`.
/// Returns the `db_storage` directory path.
pub fn build_wechat_account(dir: &Path, key: &Key) -> PathBuf {
    let storage = dir.join("db_storage");
    let _ = fs::remove_dir_all(&storage);
    fs::create_dir_all(storage.join("session")).unwrap();
    fs::create_dir_all(storage.join("message")).unwrap();
    fs::create_dir_all(storage.join("contact")).unwrap();

    // ---- session.db: Session table ----
    {
        let path = storage.join("session/session.db");
        let conn = wx_conn(&path, key, false);
        conn.execute_batch(
            "CREATE TABLE Session (
                userName TEXT PRIMARY KEY,
                displayName TEXT NOT NULL,
                sortTimeStamp INTEGER NOT NULL DEFAULT 0,
                lastTimeStamp INTEGER NOT NULL DEFAULT 0,
                lastMsg TEXT,
                lastMsgType INTEGER NOT NULL DEFAULT 0,
                unread INTEGER NOT NULL DEFAULT 0,
                type INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO Session VALUES ('wxid_friend_a', '张三', 1700000010, 1700000010, '你好', 1, 0, 0);
            INSERT INTO Session VALUES ('wxid_fake_group@chatroom', '项目群', 1700000015, 1700000015, '[图片]', 3, 2, 2);",
        )
        .unwrap();
    }

    // ---- contact.db: contact table ----
    {
        let path = storage.join("contact/contact.db");
        let conn = wx_conn(&path, key, false);
        conn.execute_batch(
            "CREATE TABLE contact (
                userName TEXT PRIMARY KEY,
                remark TEXT,
                nickName TEXT,
                alias TEXT,
                localType INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO contact VALUES ('wxid_friend_a', '客户张三', '张三', 'zhangsan001', 1);
            INSERT INTO contact VALUES ('wxid_member_b', '', '李四', '', 1);
            INSERT INTO contact VALUES ('wxid_fake_group@chatroom', '', '项目群', '', 2);",
        )
        .unwrap();
    }

    // ---- message/message_0.db: Name2Id + Msg_<md5> tables ----
    {
        let path = storage.join("message/message_0.db");
        let conn = wx_conn(&path, key, false);
        let friend_md5 = md5_hex(FAKE_FRIEND);
        let group_md5 = md5_hex(FAKE_GROUP);
        let name2id = "Name2Id0";
        conn.execute_batch(&format!(
            "CREATE TABLE \"{name2id}\" (user_name TEXT);
             INSERT INTO \"{name2id}\" (rowid, user_name) VALUES (1, 'wxid_friend_a'), (2, 'wxid_member_b'), (3, '{fake_wxid}');
             CREATE TABLE \"Msg_{friend_md5}\" (
                local_id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL,
                local_type INTEGER NOT NULL,
                create_time INTEGER NOT NULL,
                sort_seq INTEGER NOT NULL DEFAULT 0,
                real_sender_id INTEGER NOT NULL,
                message_content TEXT,
                compress_content BLOB
             );
             CREATE TABLE \"Msg_{group_md5}\" (
                local_id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL,
                local_type INTEGER NOT NULL,
                create_time INTEGER NOT NULL,
                sort_seq INTEGER NOT NULL DEFAULT 0,
                real_sender_id INTEGER NOT NULL,
                message_content TEXT,
                compress_content BLOB
             );",
            name2id = name2id,
            fake_wxid = FAKE_WXID,
            friend_md5 = friend_md5,
            group_md5 = group_md5,
        ))
        .unwrap();

        let xml_img = "<msg><img hdLength=\"0\" md5=\"aabbccddeeff00112233445566778899\"/></msg>";
        let xml_revoke = "<sysmsg type=\"revokemsg\"><revokemsg><msgid>8800000000000000001</msgid><replacemsg>对方撤回了一条消息</replacemsg></revokemsg></sysmsg>";
        for (i, (t, body, sender)) in [
            ("1", "你好，张三", "1"),
            ("1", "收到", "3"),
            ("3", xml_img, "1"),
            ("10002", xml_revoke, "1"),
        ]
        .into_iter()
        .enumerate()
        {
            conn.execute(
                &format!(
                    "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content)
                     VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                    md5 = friend_md5
                ),
                rusqlite::params![
                    8_100_000_000_000_000_000i64 + i as i64,
                    t.parse::<i64>().unwrap(),
                    1_700_000_000i64 + i as i64,
                    sender.parse::<i64>().unwrap(),
                    body
                ],
            )
            .unwrap();
        }
        let xml_img2 = "<msg><img hdLength=\"2\" md5=\"00112233445566778899aabbccddeeff\"/></msg>";
        let compressed = zstd::stream::encode_all("<msg>群消息</msg>".as_bytes(), 3).unwrap();
        for (i, (t, body, sender, compress)) in [
            ("1", "大家好", "2", None::<Vec<u8>>),
            ("1", "我发的", "3", None),
            ("3", xml_img2, "2", None),
            ("1", "", "2", Some(compressed.clone())),
        ]
        .into_iter()
        .enumerate()
        {
            conn.execute(
                &format!(
                    "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content, compress_content)
                     VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
                    md5 = group_md5
                ),
                rusqlite::params![
                    8_200_000_000_000_000_000i64 + i as i64,
                    t.parse::<i64>().unwrap(),
                    1_700_000_100i64 + i as i64,
                    sender.parse::<i64>().unwrap(),
                    body,
                    compress
                ],
            )
            .unwrap();
        }
        drop(conn);
    }

    storage
}

/// Rebuild `session.db` with the column set a real WeChat 4.x client ships:
/// `SessionTable` carries NO session-name column at all (the only name-ish
/// column is `last_sender_display_name`, i.e. the *sender* of the last
/// message, and `session_title` lives in a separate table). Probed against a
/// real 4.x account: none of the name aliases the index looks for match, so
/// `Session.display_name` stays empty and display has to fall back to
/// contacts.
pub fn rewrite_session_db_without_name_column(storage: &Path, key: &Key) {
    let path = storage.join("session/session.db");
    let _ = fs::remove_file(&path);
    let conn = wx_conn(&path, key, false);
    conn.execute_batch(
        "CREATE TABLE SessionTable (
            username TEXT PRIMARY KEY,
            type INTEGER NOT NULL DEFAULT 0,
            unread_count INTEGER NOT NULL DEFAULT 0,
            is_hidden INTEGER NOT NULL DEFAULT 0,
            summary TEXT,
            status INTEGER NOT NULL DEFAULT 0,
            last_timestamp INTEGER NOT NULL DEFAULT 0,
            sort_timestamp INTEGER NOT NULL DEFAULT 0,
            last_msg_type INTEGER NOT NULL DEFAULT 0,
            last_msg_sender TEXT,
            last_sender_display_name TEXT
         );
         INSERT INTO SessionTable
            (username, type, unread_count, summary, last_timestamp, sort_timestamp, last_msg_type, last_sender_display_name)
         VALUES
            ('wxid_friend_a', 0, 0, '你好', 1700000010, 1700000010, 1, '张三'),
            ('wxid_fake_group@chatroom', 2, 2, '[图片]', 1700000015, 1700000015, 3, '李四');
         CREATE TABLE SessionNoContactInfoTable (username TEXT PRIMARY KEY, session_title TEXT);",
    )
    .unwrap();
    drop(conn);
}

/// Add one more message row to the group conversation (a simulated WeChat
/// write). If `keep_open`, the connection stays alive so DELETE-mode data
/// lands in the main file (default); WAL-mode tests keep their own conn.
pub fn append_group_message(storage: &Path, key: &Key) {
    let path = storage.join("message/message_0.db");
    let conn = wx_conn(&path, key, false);
    let group_md5 = md5_hex(FAKE_GROUP);
    conn.execute(
        &format!(
            "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content)
             VALUES (?1, 1, ?2, 0, 2, '新消息')",
            md5 = group_md5
        ),
        rusqlite::params![8_299_999_999_999_999_999i64, 1_700_000_200i64],
    )
    .unwrap();
    drop(conn);
}

pub fn md5_hex(s: &str) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Assert the *decrypted* layout matches the WeChat 4.0 contract.
pub fn assert_wechat_layout(decrypted: &[u8]) {
    assert!(decrypted.len() >= wcdb::PAGE_SIZE);
    assert_eq!(&decrypted[..16], wcdb::SQLITE_HDR, "page 1 magic after decrypt");
    assert_eq!(decrypted[20], 80, "reserved byte must be 80");
    for chunk in decrypted.chunks(wcdb::PAGE_SIZE) {
        assert!(
            chunk[wcdb::USABLE_SIZE..].iter().all(|b| *b == 0),
            "reserved zone must be zero after decrypt"
        );
    }
}

#[allow(dead_code)]
pub fn fill_rand(buf: &mut [u8]) {
    rand::thread_rng().fill_bytes(buf);
}
