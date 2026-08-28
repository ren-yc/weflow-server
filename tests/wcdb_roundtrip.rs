//! WCDB page-cipher roundtrip arbitration tests.
//!
//! These prove the crypto layer against *real* SQLCipher output (the bundled
//! rusqlite-sqlcipher produces WeChat-4.0-compatible files: AES-256-CBC,
//! HMAC-SHA512, reserve=80, salt in page 1):
//! - a SQLCipher-created database must decrypt with `db::wcdb::decrypt_db`
//!   and reopen in SQLite with intact tables/rows
//! - a wrong key must fail deterministically
//! - a live WAL (kept-open writer, uncheckpointed frames) must patch the
//!   decrypted snapshot so the newest rows are visible

mod common;

use std::fs;

use rusqlite::Connection;
use weflow_server::db::scan;
use weflow_server::db::wcdb;
use weflow_server::keystore;

fn fake_key() -> wcdb::Key {
    keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap().0
}

#[test]
fn sqlcipher_db_decrypts_and_reopens() {
    let dir = common::tmp_dir("roundtrip");
    let key = fake_key();
    let storage = common::build_wechat_account(&dir, &key);

    // decrypt session.db with our pure-Rust page cipher
    let enc = fs::read(storage.join("session/session.db")).unwrap();
    let plain = wcdb::decrypt_db(&key, &enc).unwrap();
    common::assert_wechat_layout(&plain);

    let out = dir.join("session.dec.db");
    fs::write(&out, &plain).unwrap();
    let conn = Connection::open(&out).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let sessions: i64 = conn
        .query_row("SELECT count(*) FROM Session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 2);
    let name: String = conn
        .query_row("SELECT displayName FROM Session WHERE userName='wxid_fake_group@chatroom'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "项目群");

    // wrong key fails the page-1 HMAC deterministically
    let wrong = [9u8; 32];
    assert!(wcdb::decrypt_db(&wrong, &enc).is_err());

    // random garbage is not a WeChat db
    let mut blob = vec![0u8; 8192];
    common::fill_rand(&mut blob);
    assert!(wcdb::decrypt_db(&key, &blob).is_err());

    // the whole account decrypts (message + contact dbs too)
    for rel in ["message/message_0.db", "contact/contact.db"] {
        let enc = fs::read(storage.join(rel)).unwrap();
        let plain = wcdb::decrypt_db(&key, &enc).unwrap();
        common::assert_wechat_layout(&plain);
    }
}

#[test]
fn live_wal_frames_patch_the_snapshot() {
    let dir = common::tmp_dir("walpatch");
    let key = fake_key();
    let storage = dir.join("db_storage");
    fs::create_dir_all(storage.join("message")).unwrap();

    // WAL-mode writer that stays OPEN: frames go to -wal, uncheckpointed
    let msg_path = storage.join("message/message_0.db");
    let writer = common::wx_conn(&msg_path, &key, true);
    writer
        .execute_batch(&format!(
            "CREATE TABLE Msg_{md5} (
                local_id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL,
                local_type INTEGER NOT NULL,
                create_time INTEGER NOT NULL,
                sort_seq INTEGER NOT NULL DEFAULT 0,
                real_sender_id INTEGER NOT NULL,
                message_content TEXT
             );
             INSERT INTO Msg_{md5} (server_id, local_type, create_time, sort_seq, real_sender_id, message_content)
                VALUES (9001, 1, 1700000100, 0, 1, 'in wal'), (9002, 1, 1700000101, 0, 2, 'also in wal');",
            md5 = common::md5_hex(common::FAKE_GROUP)
        ))
        .unwrap();
    // ensure -wal exists with frames
    let wal_path = format!("{}-wal", msg_path.display());
    assert!(fs::metadata(&wal_path).is_ok(), "wal file must exist");

    // decrypt main + wal frames with our pure-Rust page cipher, then merge
    let files = scan::enum_db_files(&storage);
    assert_eq!(files.len(), 1);
    assert!(files[0].wal.is_some(), "wal sibling must be discovered");
    let enc = fs::read(&files[0].abs).unwrap();
    let wal_bytes = fs::read(files[0].wal.as_ref().unwrap()).unwrap();
    let mut plain = wcdb::decrypt_db(&key, &enc).unwrap();
    common::assert_wechat_layout(&plain);
    let frames = wcdb::decrypt_wal_frames(&key, &wal_bytes);
    assert!(!frames.is_empty(), "wal frames must decrypt");
    wcdb::apply_wal_frames(&mut plain, &frames);

    let snapshot = dir.join("patched.db");
    fs::write(&snapshot, &plain).unwrap();
    let conn = Connection::open(&snapshot).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok", "wal-patched snapshot must pass integrity");
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT count(*) FROM Msg_{}",
                common::md5_hex(common::FAKE_GROUP)
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "wal rows must be present after patching");
    drop(conn); // release the snapshot handle before re-decrypting it

    // writer still alive: another row lands in the wal; re-decrypt picks it up
    // (read-while-writing is exactly the production pattern — WeChat keeps
    // its databases open)
    writer
        .execute(
            &format!(
                "INSERT INTO Msg_{md5} (server_id, local_type, create_time, sort_seq, real_sender_id, message_content)
                 VALUES (9003, 1, 1700000102, 0, 2, 'third row')",
                md5 = common::md5_hex(common::FAKE_GROUP)
            ),
            [],
        )
        .unwrap();
    let enc = fs::read(&files[0].abs).unwrap();
    let wal_bytes = fs::read(files[0].wal.as_ref().unwrap()).unwrap();
    let mut plain = wcdb::decrypt_db(&key, &enc).unwrap();
    let frames = wcdb::decrypt_wal_frames(&key, &wal_bytes);
    wcdb::apply_wal_frames(&mut plain, &frames);
    fs::write(&snapshot, &plain).unwrap();
    let conn = Connection::open(&snapshot).unwrap();
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT count(*) FROM Msg_{}",
                common::md5_hex(common::FAKE_GROUP)
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 3, "second wal batch must also be baked in");
    drop(writer);
    drop(conn);
}

#[test]
fn empty_and_truncated_inputs_never_panic() {
    let dir = common::tmp_dir("badinputs");
    let key = fake_key();
    assert!(wcdb::decrypt_db(&key, &[]).is_err());
    assert!(wcdb::decrypt_db(&key, &[0u8; 100]).is_err());
    let storage = common::build_wechat_account(&dir, &key);
    let enc = fs::read(storage.join("contact/contact.db")).unwrap();
    // cutting a few bytes off the end: last page zero-padded; must not panic
    let _ = wcdb::decrypt_db(&key, &enc[..enc.len() - 7]);
    assert!(wcdb::decrypt_wal_frames(&key, &[]).is_empty());
    assert!(wcdb::decrypt_wal_frames(&key, &[0u8; 16]).is_empty());
    let garbage = vec![0xAAu8; 32 + 2 * wcdb::WAL_FRAME_SIZE];
    assert!(wcdb::decrypt_wal_frames(&key, &garbage).is_empty());
}
