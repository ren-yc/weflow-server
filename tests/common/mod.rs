//! Shared test fixtures: build *real* SQLCipher-encrypted databases (WeChat
//! 4.0-compatible: AES-256-CBC, HMAC-SHA512, reserve=80, 4096B pages, salt in
//! page 1) using the bundled rusqlite-sqlcipher, then feed them through
//! `db::wcdb::decrypt_db` — the ground-truth interop arbitration.

// Every integration-test binary compiles its own copy of this module, so any
// fixture not used by *all* of them reads as dead there. That is the test
// harness's compilation model, not an unused-code problem — suppressing it here
// is what keeps `-D warnings` usable as a gate.
#![allow(dead_code)]

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
        // 群元数据（群主与名册）住 contact.db。真库里它们是 `chat_room(room_id, owner)` 与
        // `chatroom_member(room_id, member_id)`，两个 id 都要用**本库**的 `name2id` 解析。
        conn.execute_batch(
            r#"CREATE TABLE chat_room (room_id INTEGER, owner INTEGER);
            CREATE TABLE chatroom_member (room_id INTEGER, member_id INTEGER);
            -- 本库的 id 空间：群 = 3、member_b = 2。**故意与 fts 库不同**（见下一节），
            -- 这样用错库的 name2id 会解析到错的人，而不是解析失败。
            CREATE TABLE "Name2Id_contact" (user_name TEXT);
            INSERT INTO "Name2Id_contact" (rowid, user_name) VALUES
                (1, 'wxid_friend_a'), (2, 'wxid_member_b'),
                (3, 'wxid_fake_group@chatroom'), (4, 'wxid_fake000000000000001');
            -- 群主给 member_b —— 断言要看到每群恰一人为 true。
            INSERT INTO chat_room VALUES (3, 2);
            INSERT INTO chatroom_member VALUES (3, 1), (3, 2);"#,
        )
        .unwrap();
    }

    // ---- contact/contact_fts.db: 群名片的来源（FTS 影子表）----
    //
    // 真库里那张 `chatroom_member_fts_v3` 是**带 WeChat 自定义分词器**的 FTS5 虚拟表，普通
    // SQLite 建不出来 —— 所以这里造它的**影子表**（普通表，形状与真库一致：`_content` 是
    // (id, c0, c1, c2)、`_aux` 是 (room_id, member_id)，两者按 rowid 连接）。生产代码读的
    // 也正是影子表，于是夹具路径与真库路径走的是同一条代码。
    {
        let path = storage.join("contact/contact_fts.db");
        let conn = wx_conn(&path, key, false);
        conn.execute_batch(
            r#"CREATE TABLE "Name2Id_fts" (user_name TEXT);
            -- **本库的 id 空间与 contact.db 不同**（11/12 vs 3/2）—— 这是真库的实测形态，
            -- 也是这段代码最容易写错的地方：拿错库的表名不会报错，只会解析到错误的人。
            INSERT INTO "Name2Id_fts" (rowid, user_name) VALUES
                (11, 'wxid_fake_group@chatroom'), (12, 'wxid_member_b'), (13, 'wxid_friend_a');
            CREATE TABLE chatroom_member_fts_v3_content (id INTEGER PRIMARY KEY, c0, c1, c2);
            CREATE TABLE chatroom_member_fts_v3_aux (room_id INTEGER, member_id INTEGER);
            -- c0 是 FTS 表的第一列 a_group_remark，即群名片。
            INSERT INTO chatroom_member_fts_v3_content (rowid, c0, c1, c2) VALUES (1, '四哥', 11, 12);
            INSERT INTO chatroom_member_fts_v3_aux (rowid, room_id, member_id) VALUES (1, 11, 12);"#,
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

/// Add one reply row to the group conversation, quoting `quoted_server_id`.
///
/// WeChat renders a quote as an appmsg whose `<refermsg>` carries the parent's
/// `<svrid>` — the very value the API exposes as `platformMessageId`, which is
/// what makes the reference resolvable by a client instead of being a dead id.
/// `append_group_message`, but with a caller-supplied suffix so repeated calls
/// produce **distinct** `server_id`s.
///
/// The plain helper hardcodes one id, which is fine for a test that appends once.
/// Anything that appends more than once (a harness several cases share) needs
/// distinct ids: two rows carrying the same `platformMessageId` are a real
/// contract violation, and a conformance case is right to flag them.
pub fn append_group_message_unique(storage: &Path, key: &Key, seq: i64) {
    let path = storage.join("message/message_0.db");
    let conn = wx_conn(&path, key, false);
    let group_md5 = md5_hex(FAKE_GROUP);
    conn.execute(
        &format!(
            "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content) \
             VALUES (?1, 1, ?2, 0, 2, ?3)",
            md5 = group_md5
        ),
        rusqlite::params![
            8_400_000_000_000_000_000i64 + seq,
            1_700_000_200i64,
            format!("一致性套件新增-{seq}")
        ],
    )
    .unwrap();
    drop(conn);
}

pub fn append_group_reply(storage: &Path, key: &Key, quoted_server_id: i64) {
    let path = storage.join("message/message_0.db");
    let conn = wx_conn(&path, key, false);
    let group_md5 = md5_hex(FAKE_GROUP);
    let content = format!(
        "<msg><appmsg><type>57</type><title>回复</title>\
         <refermsg><type>1</type><svrid>{quoted_server_id}</svrid><content>大家好</content></refermsg>\
         </appmsg></msg>"
    );
    conn.execute(
        &format!(
            "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content)
             VALUES (?1, 49, ?2, 0, 2, ?3)",
            md5 = group_md5
        ),
        rusqlite::params![8_200_000_000_000_000_099i64, 1_700_000_109i64, content],
    )
    .unwrap();
    drop(conn);
}

/// Add `count` messages to the group conversation that share **one**
/// `create_time` but differ in `sort_seq`.
///
/// Why the fixture needs this: the Pull face pages on whole timestamp groups,
/// so a same-second burst is the case where a naive cursor either splits a group
/// (skipping or repeating rows) or loops forever. Without such rows in the fake
/// DB the conformance cases that pin that behaviour have nothing to exercise —
/// they would pass against an implementation that gets it wrong.
pub fn append_same_second_burst(storage: &Path, key: &Key, count: i64, create_time: i64) {
    let path = storage.join("message/message_0.db");
    let conn = wx_conn(&path, key, false);
    let group_md5 = md5_hex(FAKE_GROUP);
    for i in 0..count {
        conn.execute(
            &format!(
                "INSERT INTO \"Msg_{md5}\" (server_id, local_type, create_time, sort_seq, real_sender_id, message_content) \
                 VALUES (?1, 1, ?2, ?3, 2, ?4)",
                md5 = group_md5
            ),
            rusqlite::params![
                8_300_000_000_000_000_000i64 + i,
                create_time,
                i,
                format!("同秒消息{i}")
            ],
        )
        .unwrap();
    }
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
