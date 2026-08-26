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
