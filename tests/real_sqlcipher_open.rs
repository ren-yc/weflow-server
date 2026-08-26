//! Discriminator: open the real DB with the SAME bundled SQLCipher used by
//! our tests (rusqlite-sqlcipher). If SQLCipher itself can open it with the
//! registered key, the format is standard and only the key matters; if it
//! fails, the format deviates from stock SQLCipher 4 and we must find the
//! WCDB-specific variant.

use std::env;
use std::fs;
use rusqlite::Connection;

#[test]
fn stock_sqlcipher_wal_salt_relationship() {
    // control experiment: how does STOCK sqlcipher tie the WAL header salt to
    // the database salt? (test whether the real-account mismatch is abnormal)
    let work = env::temp_dir().join(format!("wfs-walsalt-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let db_path = work.join("t.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA cipher_page_size = 4096;
         PRAGMA key = \"x'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'\";
         PRAGMA journal_mode = WAL;
         CREATE TABLE t (a);",
    )
    .unwrap();
    conn.execute("INSERT INTO t VALUES (1)", []).unwrap();
    let db = fs::read(&db_path).unwrap();
    let wal = fs::read(format!("{}-wal", db_path.display())).unwrap();
    println!(
        "stock: dbsalt={} walsalt={}",
        hex::encode(&db[..16]),
        hex::encode(&wal[16..24])
    );
    drop(conn);
    let _ = fs::remove_dir_all(&work);
}

#[test]
#[ignore = "requires a real WeChat 4.0 account (WEFLOW_TEST_DB_ROOT/WEFLOW_TEST_KEY)"]
fn open_with_stock_sqlcipher() {
    let Some(root) = env::var("WEFLOW_TEST_DB_ROOT").ok() else { return };
    let Some(key_hex) = env::var("WEFLOW_TEST_KEY").ok() else { return };
    let db_path = format!("{}/db_storage/session/session.db", root.trim_end_matches('/'));
    // copy to a fresh name so sqlcipher never touches the original/copy dbs
    let work = env::temp_dir().join(format!("wfs-open-probe-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let probe = work.join("session.db");
    fs::copy(&db_path, &probe).unwrap();

    let variants: Vec<(&str, &str)> = vec![
        ("compat4-defaults", "PRAGMA cipher_compatibility = 4;"),
        ("raw", ""),
        ("pass-hex", ""),
        ("pass-hex-kdf64000sha512", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;\nPRAGMA kdf_iter = 64000;"),
        ("pass-hex-kdf256000sha1", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA1;\nPRAGMA kdf_iter = 256000;"),
        ("pass-hex-kdf64000sha256", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA256;\nPRAGMA kdf_iter = 64000;"),
        ("pass-hex-kdf256000sha256", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA256;\nPRAGMA kdf_iter = 256000;"),
        ("pass-hex-kdf100000sha512", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;\nPRAGMA kdf_iter = 100000;"),
        ("pass-hex-kdf100000sha1", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA1;\nPRAGMA kdf_iter = 100000;"),
        ("pass-hex-kdf1sha512-hmacsha1", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512;\nPRAGMA kdf_iter = 1;\nPRAGMA cipher_hmac_algorithm = HMAC_SHA1;"),
        ("pass-hex-kdf64000sha1-hmacsha512", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA1;\nPRAGMA kdf_iter = 64000;\nPRAGMA cipher_hmac_algorithm = HMAC_SHA512;"),
        ("pass-hex-kdf64000sha1-hmacsha1", "PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA1;\nPRAGMA kdf_iter = 64000;\nPRAGMA cipher_hmac_algorithm = HMAC_SHA1;"),
        ("pass-hex-compat3", "PRAGMA cipher_compatibility = 3;"),
        ("raw-compat3", "PRAGMA cipher_compatibility = 3;"),
    ];
    for (name, extra) in variants {
        let conn = Connection::open(&probe);
        let mut conn = match conn {
            Ok(c) => c,
            Err(e) => {
                println!("{name}: open failed {e}");
                continue;
            }
        };
        let key_stmt = if name.starts_with("pass-") {
            format!("PRAGMA key = '{key_hex}';")
        } else {
            format!("PRAGMA key = \"x'{key_hex}'\";")
        };
        let sql = format!("PRAGMA cipher_page_size = 4096;\n{key_stmt}\n{extra}");
        let r1 = conn.execute_batch(&sql);
        let r2 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0));
        println!("{name}: key-set {r1:?} schema-count {r2:?}");
        drop(conn);
        let _ = fs::remove_file(&probe);
        fs::copy(&db_path, &probe).unwrap();
    }
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_dir_all(&work);
}