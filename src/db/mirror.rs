//! Decrypted mirror: write decrypted copies of the encrypted WeChat 4.0
//! databases into `<mirror>/<wxid>/<rel>`, patching WAL frames in.
//!
//! Strategy (v1):
//! - full decrypt on first registration (all `.db` files under `db_storage`)
//! - afterwards only files whose source mtime/size changed since the last
//!   pass are re-decrypted (manifest-driven), so `message_*.db`-heavy accounts
//!   do not pay a full re-decrypt on every file event
//! - output is written atomically (temp file + rename)
//! - every failure is local: one broken db file must not abort the account

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::scan::DbFile;
use super::wcdb;
use crate::keystore::KeyMap;

/// Source fingerprint used to skip unchanged files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime_ns: i128,
    size: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Stamp> {
        let md = fs::metadata(path).ok()?;
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        Some(Stamp { mtime_ns, size: md.len() })
    }
}

/// Decrypt one encrypted db file (plus its WAL frames) into `out`.
/// `out` is written atomically (tmp + rename).
pub fn decrypt_to(out: &Path, encrypted: &[u8], wal: Option<&Path>, key: &[u8; 32]) -> Result<()> {
    let mut snapshot = wcdb::decrypt_db(key, encrypted)?;
    if let Some(wal_path) = wal
        && let Ok(wal_bytes) = fs::read(wal_path) {
            let frames = wcdb::decrypt_wal_frames(key, &wal_bytes);
            wcdb::apply_wal_frames(&mut snapshot, &frames);
        }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("tmp-dec");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&snapshot)?;
        f.sync_all()?;
    }
    // Windows: rename cannot overwrite an existing destination — drop it first
    if out.exists() {
        fs::remove_file(out)?;
    }
    fs::rename(&tmp, out)?;
    Ok(())
}

/// Mirror state for one account: source stamps + snapshot paths.
pub struct Mirror {
    /// Where decrypted files live: `<mirror_root>/<wxid>/<rel>`
    pub root: PathBuf,
    /// rel path -> main-file source stamp (persistent across syncs)
    stamps: HashMap<String, Stamp>,
    /// rel path -> wal stamp we last applied (persistent across syncs)
    wal_stamps: HashMap<String, Stamp>,
}

impl Mirror {
    pub fn new(mirror_root: &Path, wxid: &str) -> Self {
        Mirror {
            root: mirror_root.join(wxid),
            stamps: HashMap::new(),
            wal_stamps: HashMap::new(),
        }
    }

    pub fn snapshot_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Refresh the mirror for a set of live db files. Returns the relative
    /// paths that were (re)decrypted this pass.
    ///
    /// Returns `(changed, errors)`; errors are per-file and non-fatal.
    pub fn refresh(
        &mut self,
        files: &[DbFile],
        keys: &KeyMap,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut changed = Vec::new();
        let mut errors = Vec::new();
        for f in files {
            let Some(key) = keys.key_for(&f.rel) else {
                errors.push((f.rel.clone(), "no key registered for this db".into()));
                continue;
            };
            let Some(stamp) = Stamp::of(&f.abs) else {
                errors.push((f.rel.clone(), "source unreadable".into()));
                continue;
            };
            let wal_stamp = f.wal.as_ref().and_then(|w| Stamp::of(w));
            // Skip only when BOTH the main file and the wal we last applied
            // are unchanged — WeChat 4.x keeps a preallocated 4MB -wal that
            // is rewritten constantly, so a wal-only change must trigger a
            // re-decrypt, but an untouched wal must NOT force a full pass.
            let wal_same = if f.wal.is_none() {
                true
            } else if let Some(ws) = &wal_stamp {
                self.wal_stamps.get(&f.rel).is_some_and(|s| s == ws)
            } else {
                false
            };
            let skip = self
                .stamps
                .get(&f.rel)
                .is_some_and(|s| *s == stamp)
                && self.snapshot_path(&f.rel).is_file()
                && wal_same;
            if skip {
                continue;
            }
            match fs::read(&f.abs)
            .map_err(|e| format!("read {}: {e}", f.abs.display()))
        {
                Ok(enc) => {
                    let out = self.snapshot_path(&f.rel);
                    let res = decrypt_to(&out, &enc, f.wal.as_deref(), &key.0);
                    match res {
                        Ok(()) => {
                            self.stamps.insert(f.rel.clone(), stamp);
                            if let Some(ws) = wal_stamp {
                                self.wal_stamps.insert(f.rel.clone(), ws);
                            }
                            changed.push(f.rel.clone());
                        }
                        Err(e) => errors.push((f.rel.clone(), e.to_string())),
                    }
                }
                Err(e) => {
                    // Transient lock contention (WeChat / SQLite mid-write,
                    // antivirus): retry a few times before giving up.
                    let mut last = e;
                    let mut ok = false;
                    for _ in 0..3 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        match fs::read(&f.abs).map_err(|e| format!("read {}: {e}", f.abs.display())) {
                            Ok(enc) => {
                                let out = self.snapshot_path(&f.rel);
                                match decrypt_to(&out, &enc, f.wal.as_deref(), &key.0) {
                                    Ok(()) => {
                                        self.stamps.insert(f.rel.clone(), stamp);
                                        if let Some(ws) = wal_stamp {
                                            self.wal_stamps.insert(f.rel.clone(), ws);
                                        }
                                        changed.push(f.rel.clone());
                                        ok = true;
                                        break;
                                    }
                                    Err(e2) => last = e2.to_string(),
                                }
                            }
                            Err(e2) => last = e2.to_string(),
                        }
                    }
                    if !ok {
                        errors.push((f.rel.clone(), last));
                    }
                }
            }
        }
        (changed, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::parse_db_key;

    const KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Open an encrypted (SQLCipher, WeChat-4 params) connection.
    fn enc_conn(path: &Path, key_hex: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA cipher_page_size = 4096;
             PRAGMA key = \"x'{key_hex}'\";"
        ))
        .unwrap();
        conn
    }

    #[test]
    fn mirror_refresh_decrypts_and_skips_unchanged() {
        let dir = std::env::temp_dir().join(format!("wf-mirror-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("storage/session")).unwrap();
        let key = parse_db_key(KEY_HEX).unwrap();

        let db_path = dir.join("storage/session/session.db");
        {
            let conn = enc_conn(&db_path, KEY_HEX);
            conn.execute_batch(
                "CREATE TABLE Session (userName TEXT PRIMARY KEY, displayName TEXT NOT NULL, lastTimeStamp INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO Session VALUES ('wxid_a', '张三', 1700000001),
                                            ('wxid_b@chatroom', '群', 1700000002);",
            )
            .unwrap();
        }

        let mut mirror = Mirror::new(&dir.join("mirror"), "wxid_test");
        let files = vec![DbFile {
            rel: "session/session.db".into(),
            abs: db_path.clone(),
            wal: None,
            kind: crate::db::scan::DbKind::Session,
        }];
        let (changed, errors) = mirror.refresh(&files, &crate::keystore::KeyMap::Single(key));
        assert!(errors.is_empty());
        assert_eq!(changed, vec!["session/session.db"]);
        let snap = mirror.snapshot_path("session/session.db");
        let bytes = fs::read(&snap).unwrap();
        assert_eq!(&bytes[..16], wcdb::SQLITE_HDR);
        assert_eq!(bytes[20], 80);
        // the decrypted snapshot opens as a plain database
        let conn = rusqlite::Connection::open(&snap).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM Session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        // second pass: unchanged -> no work (metadata-only skip)
        let (changed2, errors2) = mirror.refresh(&files, &crate::keystore::KeyMap::Single(key));
        assert!(errors2.is_empty());
        assert!(changed2.is_empty());

        // wrong key -> per-file error, no panic (fresh mirror without stamps)
        let bad_key = crate::keystore::DbKey([9u8; 32]);
        let mut mirror_bad = Mirror::new(&dir.join("mirror"), "wxid_test");
        let (changed3, errors3) = mirror_bad.refresh(&files, &crate::keystore::KeyMap::Single(bad_key));
        assert!(changed3.is_empty());
        assert_eq!(errors3.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}