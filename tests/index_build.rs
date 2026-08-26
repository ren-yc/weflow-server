//! Index build + incremental-read integration tests over the WeChat-like
//! encrypted fixture account: live connections -> full_sync -> store,
//! then watermark-incremental polls pick up appended rows.

mod common;

use std::sync::{Arc};

use parking_lot::RwLock;

use weflow_server::db::scan;
use weflow_server::db::wcdb;
use weflow_server::keystore;
use weflow_server::store::{SessionKind, Store, Watermark};
use weflow_server::sync::AccountSync;

fn fake_key() -> wcdb::Key {
    keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap().0
}

fn km(key: wcdb::Key) -> weflow_server::keystore::KeyMap {
    weflow_server::keystore::KeyMap::from(keystore::DbKey(key))
}

/// Build the fixture account and run a full live sync. Returns the sync
/// engine (for polls) plus a handle on the shared store.
fn make_sync(dir: &std::path::Path) -> (AccountSync, Arc<RwLock<Store>>) {
    let key = fake_key();
    let storage = common::build_wechat_account(dir, &key);
    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        km(key),
        store.clone(),
    );
    let n = sync.full_sync().unwrap();
    assert!(n > 0, "fixture must contain databases");
    (sync, store)
}

#[test]
fn index_builds_sessions_contacts_and_messages() {
    let dir = common::tmp_dir("indexbuild");
    let (_sync, store) = make_sync(&dir);
    let store = store.read();

    // sessions from SessionTable
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
}

#[test]
fn incremental_poll_picks_up_new_rows_after_watermark() {
    let dir = common::tmp_dir("liveincr");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);
    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        km(key),
        store.clone(),
    );
    sync.full_sync().unwrap();

    // simulated WeChat write: a new group message lands in the source
    if std::path::Path::new(&storage.join("message")).exists() {
        for e in std::fs::read_dir(storage.join("message")).unwrap() {
            let e = e.unwrap();
            eprintln!("DBGFILE {:?} len={} mtime={:?}", e.file_name(), e.metadata().unwrap().len(), e.metadata().unwrap().modified());
        }
    }
    common::append_group_message(&storage, &key);
    {
        let path = storage.join("message/message_0.db");
        let conn = common::wx_conn(&path, &key, false);
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM \"Msg_{}\"", common::md5_hex(common::FAKE_GROUP)),
            [], |r| r.get(0)).unwrap();
        eprintln!("AFTER-APPEND COUNT={}", n);
        drop(conn);
        for e in std::fs::read_dir(storage.join("message")).unwrap() {
            let e = e.unwrap();
            eprintln!("DBGFILE-AFTER {:?} len={} mtime={:?}", e.file_name(), e.metadata().unwrap().len(), e.metadata().unwrap().modified());
        }
    }

    let (n, revokes) = sync.poll_once().unwrap();
    assert_eq!(n, 1, "one new row picked up");
    assert_eq!(revokes, 0);

    let guard = store.read();
    let msgs = guard.convs.get(common::FAKE_GROUP).unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last.create_time, 1_700_000_200);
    assert_eq!(last.parsed.parsed_text, "新消息");

    // watermark advanced past the new row
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(common::FAKE_GROUP.as_bytes());
    let group_md5 = format!("{:x}", h.finalize());
    let wm_key_prefix = format!("message/message_0.db:Msg_{group_md5}");
    let wm = guard
        .watermarks
        .iter()
        .find(|(k, _)| k.starts_with(&wm_key_prefix))
        .map(|(_, w)| *w)
        .expect("watermark recorded");
    assert!(wm.create_time >= 1_700_000_200);
}

#[test]
fn missing_tables_degrade_gracefully() {
    let dir = common::tmp_dir("livedegrade");
    let key = fake_key();
    // storage dir containing only an unrelated empty db file
    let storage = dir.join("db_storage");
    std::fs::create_dir_all(storage.join("session")).unwrap();
    let db_path = storage.join("session/empty.db");
    std::fs::write(&db_path, b"").unwrap();

    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        km(key),
        store.clone(),
    );
    // full_sync over an empty db must not panic; store stays empty
    let _ = sync.full_sync();
    let guard = store.read();
    assert!(guard.is_empty());
}

#[test]
fn wrong_key_is_safely_degraded_at_open() {
    let dir = common::tmp_dir("livewrongkey");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);
    let bad = weflow_server::keystore::KeyMap::from(keystore::DbKey([9u8; 32]));
    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        bad,
        store.clone(),
    );
    // resilient design: a wrong key degrades to an empty index instead of
    // panicking; the registration layer pre-rejects wrong keys via page-1 HMAC
    let _ = sync.full_sync();
    assert!(store.read().is_empty(), "wrong key must yield no data");
    // and polling stays cheap/quiet
    let (n, r) = sync.poll_once().unwrap();
    assert_eq!((n, r), (0, 0));
}

#[test]
fn session_table_missing_still_indexes_messages() {
    let dir = common::tmp_dir("indexnosession");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);
    // butcher session.db: same key, but no SessionTable
    let enc_path = storage.join("session/session.db");
    {
        let _ = std::fs::remove_file(&enc_path);
        let conn = common::wx_conn(&enc_path, &key, false);
        conn.execute_batch(
            "CREATE TABLE other (x INTEGER);
             INSERT INTO other VALUES (1);",
        )
        .unwrap();
    }
    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        km(key),
        store.clone(),
    );
    sync.full_sync().unwrap();
    // sessions degrade to zero but messages still indexed
    let guard = store.read();
    assert_eq!(guard.sessions.len(), 0);
    assert_eq!(guard.convs.len(), 2);
}
