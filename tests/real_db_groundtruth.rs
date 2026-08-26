//! Ground-truth probes against a REAL WeChat 4.0 account.
//!
//! Skipped by default. Enable with:
//!   WEFLOW_TEST_DB_ROOT=<account dir containing db_storage>
//!   WEFLOW_TEST_KEY=<session.db 的 64-hex enc_key>
//!   cargo test --test real_db_groundtruth -- --ignored
//!
//! These validate the page cipher, column probing, session/contact/message
//! mapping and incremental reads against a live (or copied) WeChat account.
//! All output is fake-safe: real identifiers never enter the repo.

mod common;

use std::env;
use std::fs;
use std::path::PathBuf;

use weflow_server::db::mirror::Mirror;
use weflow_server::db::scan;
use weflow_server::db::wcdb;
use weflow_server::keystore;
use weflow_server::store::index;

/// Load the real-db key source: either a single key (`WEFLOW_TEST_KEY`) or a
/// per-database key map (`WEFLOW_TEST_KEYS_JSON`, the `all_keys.json` format
/// produced by wechat-decrypt: { "session\\session.db": {"enc_key": "..."} }).
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
                let hex = entry
                    .get("enc_key")
                    .and_then(|e| e.as_str())
                    .unwrap_or_default()
                    .to_string();
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

fn session_key(keys: &keystore::KeyMap, root: &std::path::Path) -> (wcdb::Key, String) {
    let rel = "session/session.db";
    let key = keys
        .key_for(rel)
        .or_else(|| {
            let storage = root.join("db_storage");
            let f = scan::enum_db_files(&storage)
                .into_iter()
                .find(|f| f.kind == weflow_server::db::scan::DbKind::Session);
            f.as_ref().and_then(|f| keys.key_for(&f.rel))
        })
        .expect("session key in map");
    (key.0, rel.to_string())
}

#[test]
#[ignore = "requires a real WeChat 4.0 account (WEFLOW_TEST_DB_ROOT/WEFLOW_TEST_KEY)"]
fn real_session_db_roundtrips() {
    let Some((root, keys)) = real_env() else {
        return;
    };
    let session = root.join("db_storage/session/session.db");
    if !session.is_file() {
        panic!("no session.db at {}", session.display());
    }
    let (key, _rel) = session_key(&keys, &root);
    let enc = fs::read(&session).unwrap();
    let plain = wcdb::decrypt_db(&key, &enc).expect("decrypt with the registered key");
    common::assert_wechat_layout(&plain);
}

#[test]
#[ignore = "requires a real WeChat 4.0 account (WEFLOW_TEST_DB_ROOT/WEFLOW_TEST_KEY)"]
fn real_account_indexes() {
    let Some((root, keys)) = real_env() else {
        return;
    };
    let storage = root.join("db_storage");
    let files = scan::enum_db_files(&storage);
    assert!(!files.is_empty(), "no databases under db_storage");
    let mut mirror = Mirror::new(&root.join(".wfs-mirror"), "real");
    let (changed, errors) = mirror.refresh(&files, &keys);
    eprintln!("decrypted {} files; errors: {}", changed.len(), errors.len());
    for (rel, e) in errors.iter().take(20) {
        eprintln!("  error {rel}: {e}");
    }
    let store = index::build_all(&mirror.root, "real", &files).unwrap();
    eprintln!(
        "index: {} sessions, {} conversations, {} contacts, {} message tables",
        store.sessions.len(),
        store.convs.len(),
        store.contacts.len(),
        store.watermarks.len()
    );
    assert!(
        !store.sessions.is_empty() || !store.convs.is_empty(),
        "expected at least some indexed data"
    );
}

/// Full-database probe (privacy-safe: counts and schema shapes only, never
/// message contents or other people's identifiers).
#[test]
#[ignore = "requires a real WeChat 4.0 account (WEFLOW_TEST_DB_ROOT/WEFLOW_TEST_KEY)"]
fn real_db_full_probe() {
    let Some((root, keys)) = real_env() else {
        return;
    };
    let storage = root.join("db_storage");
    let files = scan::enum_db_files(&storage);

    // 1) per-file: HMAC verify + full decrypt + reopen + table inventory
    eprintln!("== decrypt probe: {} db files ==", files.len());
    let mut matched = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut pattern_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let probe_dir = root.join(".wfs-probe");
    fs::create_dir_all(&probe_dir).unwrap();
    for f in &files {
        let enc = match fs::read(&f.abs) {
            Ok(e) => e,
            Err(e) => {
                failed.push((f.rel.clone(), format!("read: {e}")));
                continue;
            }
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
        let tmp = probe_dir.join(f.rel.replace('/', "_").replace('\\', "_"));
        fs::write(&tmp, &plain).unwrap();
        if let Ok(conn) = weflow_server::db::open::open_snapshot(&tmp) {
            let integrity: String = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap_or_else(|_| "ERR".into());
            let tables = weflow_server::db::open::table_names(&conn, 200);
            let msg_tables = tables
                .iter()
                .filter(|t| {
                    let l = t.to_ascii_lowercase();
                    l.starts_with("msg_") && l.len() >= 20
                })
                .count();
            let n2id = tables
                .iter()
                .filter(|t| t.to_ascii_lowercase().starts_with("name2id"))
                .count();
            let session_tables = tables
                .iter()
                .filter(|t| t.eq_ignore_ascii_case("Session"))
                .count();
            eprintln!(
                "{:>12}  {:<44} integrity={:<2} tables={:<3} msg_*={} name2id={} session={}",
                f.abs.metadata().map(|m| m.len()).unwrap_or(0),
                f.rel,
                integrity,
                tables.len(),
                msg_tables,
                n2id,
                session_tables
            );
            let pattern = if msg_tables > 0 {
                "Msg_<md5>"
            } else if n2id > 0 {
                "Name2Id"
            } else if session_tables > 0 {
                "Session"
            } else {
                "other"
            };
            *pattern_counts.entry(pattern.to_string()).or_insert(0) += 1;
        } else {
            failed.push((f.rel.clone(), "decrypted db failed to open".into()));
        }
        let _ = fs::remove_file(&tmp);
    }
    eprintln!("HMAC matched: {matched}/{}", files.len());
    for (rel, e) in &failed {
        eprintln!("  FAIL {rel}: {e}");
    }
    for (p, n) in &pattern_counts {
        eprintln!("  pattern {p}: {n} dbs");
    }

    // 2) mirror (with WAL patching) + full index
    eprintln!("== mirror + index ==");
    let mut mirror = Mirror::new(&root.join(".wfs-mirror2"), "real");
    let (changed, errors) = mirror.refresh(&files, &keys);
    eprintln!(
        "mirror: refreshed {} files, {} per-file errors",
        changed.len(),
        errors.len()
    );
    for (rel, e) in errors.iter().take(30) {
        eprintln!("  mirror error {rel}: {e}");
    }
    let store = index::build_all(&mirror.root, "real", &files).unwrap();
    eprintln!(
        "index: sessions={} convs={} contacts={} watermarked tables={}",
        store.sessions.len(),
        store.convs.len(),
        store.contacts.len(),
        store.watermarks.len()
    );
    for (tbl, _wm) in store.watermarks.iter().take(20) {
        eprintln!("  watermark {tbl}");
    }
    let mut total_msgs = 0usize;
    let mut conv_sizes: Vec<(String, usize)> = store
        .convs
        .iter()
        .map(|(k, v)| {
            total_msgs += v.len();
            (if k.len() > 12 { format!("{}…", &k[..12]) } else { k.clone() }, v.len())
        })
        .collect();
    conv_sizes.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!(
        "conversations: {} total messages: {}",
        conv_sizes.len(),
        total_msgs
    );
    for (k, n) in conv_sizes.iter().take(15) {
        eprintln!("  {k}: {n}");
    }
    let named = store
        .convs
        .values()
        .flatten()
        .filter(|m| !m.sender_name.is_empty() && m.sender_name != m.sender_username)
        .count();
    eprintln!(
        "messages with resolved sender display names: {named}/{total_msgs}"
    );

    assert!(
        !store.sessions.is_empty() || !store.convs.is_empty(),
        "expected indexed data from the real account"
    );
}