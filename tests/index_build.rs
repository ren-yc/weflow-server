//! Index build + incremental-read integration tests over the WeChat-like
//! encrypted fixture account: mirror -> decrypt -> build_all -> store.

mod common;

use std::fs;

use weflow_server::db::mirror::Mirror;
use weflow_server::db::open;
use weflow_server::db::scan::{self, DbKind};
use weflow_server::db::wcdb;
use weflow_server::keystore;
use weflow_server::store::index::{self, read_new};
use weflow_server::store::{SessionKind, Watermark};

fn fake_key() -> wcdb::Key {
    keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap().0
}

fn build_store(dir: &std::path::Path) -> (Mirror, weflow_server::store::Store, Vec<scan::DbFile>) {
    let key = fake_key();
    let storage = common::build_wechat_account(dir, &key);
    let files = scan::enum_db_files(&storage);
    let mut mirror = Mirror::new(&dir.join("mirror"), common::FAKE_WXID);
    let (changed, errors) = mirror.refresh(&files, &weflow_server::keystore::KeyMap::from(weflow_server::keystore::DbKey(key)));
    assert!(errors.is_empty(), "mirror refresh errors: {errors:?}");
    assert_eq!(changed.len(), files.len());
    let store = index::build_all(&mirror.root, common::FAKE_WXID, &files).unwrap();
    (mirror, store, files)
}

#[test]
fn index_builds_sessions_contacts_and_messages() {
    let dir = common::tmp_dir("indexbuild");
    let (mirror, store, files) = build_store(&dir);
    let _ = &mirror;

    // sessions from Session table
    assert_eq!(store.sessions.len(), 2);
    let friend = store.sessions.get(common::FAKE_FRIEND).unwrap();
    assert_eq!(friend.display_name, "张三");
    assert_eq!(friend.kind, SessionKind::Private);
    let group = store.sessions.get(common::FAKE_GROUP).unwrap();
    assert_eq!(group.kind, SessionKind::Group);
    assert_eq!(group.unread_count, 2);

    // contacts from contact.db
    assert_eq!(store.contacts.len(), 3);
    assert_eq!(
        store.contacts.get(common::FAKE_FRIEND).unwrap().remark.as_deref(),
        Some("客户张三")
    );

    // messages: friend 4 rows, group 4 rows, md5 tables resolved to usernames
    assert_eq!(store.convs.len(), 2);
    assert_eq!(store.convs[common::FAKE_FRIEND].len(), 4);
    assert_eq!(store.convs[common::FAKE_GROUP].len(), 4);

    // sender resolution through Name2Id: real_sender_id=1 -> wxid_friend_a
    let friend_msgs = &store.convs[common::FAKE_FRIEND];
    assert_eq!(friend_msgs[0].sender_username, "wxid_friend_a");
    // my message (sender rowid 3 = fake wxid) -> is_send
    let group_msgs = &store.convs[common::FAKE_GROUP];
    assert!(group_msgs.iter().any(|m| m.is_send && m.sender_username == common::FAKE_WXID));
    // display priority: remark wins for the friend
    let first = friend_msgs.iter().find(|m| m.local_type == 1).unwrap();
    assert_eq!(first.sender_name, "客户张三");
    // image message parsed with media hint
    let img = friend_msgs.iter().find(|m| m.local_type == 3).unwrap();
    assert_eq!(img.parsed.display, "[图片]");
    let media = img.parsed.media.as_ref().unwrap();
    assert_eq!(media.file_name, "aabbccddeeff00112233445566778899.jpg");
    // revoke parsed
    let revoke = friend_msgs.iter().find(|m| m.local_type == 10002).unwrap();
    assert!(revoke.parsed.revoke.is_some());
    // zstd compress_content decoded
    let zstd_msg = group_msgs.iter().find(|m| m.sender_username == "wxid_member_b" && m.parsed.parsed_text == "群消息");
    assert!(zstd_msg.is_some(), "zstd compress_content must decode");

    // watermarks recorded for both tables
    assert_eq!(store.watermarks.len(), 2);
    for k in store.watermarks.keys() {
        assert!(k.starts_with("message/message_0.db:Msg_") || k.starts_with("message/message_0.db:msg_"));
    }

    // session message counts filled
    let group2 = store.sessions.get(common::FAKE_GROUP).unwrap();
    assert_eq!(group2.message_count, 4);
    let _ = files;
}

#[test]
fn incremental_read_picks_up_new_rows_after_watermark() {
    let dir = common::tmp_dir("indexincr");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);
    let files = scan::enum_db_files(&storage);

    // initial build (partial: reuse full refresh)
    let mut mirror = Mirror::new(&dir.join("mirror"), common::FAKE_WXID);
    let (_, errors) = mirror.refresh(&files, &weflow_server::keystore::KeyMap::from(weflow_server::keystore::DbKey(key)));
    assert!(errors.is_empty());
    let store = index::build_all(&mirror.root, common::FAKE_WXID, &files).unwrap();

    // simulated WeChat write: new group message lands in the source
    common::append_group_message(&storage, &key);
    let files2 = scan::enum_db_files(&storage);
    let (changed, errors) = mirror.refresh(&files2, &weflow_server::keystore::KeyMap::from(weflow_server::keystore::DbKey(key)));
    assert!(errors.is_empty());
    assert_eq!(changed, vec!["message/message_0.db"]);

    // read_new on the changed table past its watermark
    let msg_file = files2.iter().find(|f| f.kind == DbKind::Message).unwrap();
    let conn = open::open_snapshot(&mirror.snapshot_path(&msg_file.rel)).unwrap();
    let tables = index::message_tables(&conn);
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(common::FAKE_GROUP.as_bytes());
    let group_md5 = format!("{:x}", h.finalize());
    let table = tables
        .iter()
        .find(|(_, s)| *s == group_md5)
        .map(|(t, _)| t.clone())
        .unwrap();
    let wm_key = format!("{}:{table}", msg_file.rel);
    let watermark = store.watermarks.get(&wm_key).copied().unwrap_or(Watermark::default());
    let new_rows = read_new(&conn, &table, &watermark, None).unwrap();
    assert_eq!(new_rows.len(), 1, "one new row after the watermark");
    assert_eq!(new_rows[0].create_time, 1_700_000_200);
    assert_eq!(new_rows[0].parsed.parsed_text, "新消息");

    // and an empty read when nothing changed
    let again = read_new(
        &conn,
        &table,
        &Watermark {
            create_time: new_rows[0].create_time,
            sort_seq: new_rows[0].sort_seq,
            local_id: new_rows[0].local_id,
        },
        None,
    )
    .unwrap();
    assert!(again.is_empty());
}

#[test]
fn missing_tables_degrade_gracefully() {
    let dir = common::tmp_dir("indexdegrade");
    let key = fake_key();
    // empty storage dir: no session.db at all
    let storage = dir.join("db_storage");
    fs::create_dir_all(storage.join("session")).unwrap();
    let db_path = storage.join("session/empty.db");
    fs::write(&db_path, b"").unwrap();
    let files = scan::enum_db_files(&storage);
    // fresh mirror with an empty file: decrypt fails per-file, index still works
    let mut mirror = Mirror::new(&dir.join("mirror"), common::FAKE_WXID);
    let (_, errors) = mirror.refresh(&files, &weflow_server::keystore::KeyMap::from(weflow_server::keystore::DbKey(key)));
    assert_eq!(errors.len(), 1, "empty db must produce a per-file error, not a panic");
    let store = index::build_all(&mirror.root, common::FAKE_WXID, &files).unwrap();
    assert!(store.is_empty());
    assert_eq!(store.sessions.len(), 0);
    assert_eq!(store.convs.len(), 0);
}

#[test]
fn session_table_missing_still_indexes_messages() {
    let dir = common::tmp_dir("indexnosession");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);
    // butcher session.db: same key, but no `Session` table
    let enc_path = storage.join("session/session.db");
    {
        let _ = fs::remove_file(&enc_path);
        let conn = common::wx_conn(&enc_path, &key, false);
        conn.execute_batch(
            "CREATE TABLE other (x INTEGER);
             INSERT INTO other VALUES (1);",
        )
        .unwrap();
    }
    let files = scan::enum_db_files(&storage);
    let mut mirror = Mirror::new(&dir.join("mirror"), common::FAKE_WXID);
    let (_, errors) = mirror.refresh(&files, &weflow_server::keystore::KeyMap::from(weflow_server::keystore::DbKey(key)));
    assert!(errors.is_empty(), "{errors:?}");
    let store = index::build_all(&mirror.root, common::FAKE_WXID, &files).unwrap();
    // sessions degrade to zero but messages still indexed
    assert_eq!(store.sessions.len(), 0);
    assert_eq!(store.convs.len(), 2);
}