//! Account discovery: locate WeChat 4.x data roots and account directories,
//! and enumerate the encrypted databases under `db_storage`.
//!
//! Layout facts (WeFlow `dbPathService.ts` / `accountDirResolver.ts` cross-read):
//! - root: `Documents\xwechat_files` (Windows); account dir = `<wxid>` or
//!   `<custom_id>_<4digit>`; an account dir contains `db_storage/`
//! - when `wxid_X` and `wxid_X_xxxx` coexist, prefer the one containing
//!   `session.db` / the newest / the suffixed one
//! - `db_storage/`: session/session.db, message/message_*.db, contact/contact.db,
//!   emoticon/, sns/, hardlink/, media_*.db
//! - `-wal` files are preallocated to a fixed 4 MB next to their `.db`

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Default WeChat 4.x data root on this platform.
pub fn default_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        if let Some(docs) = dirs::document_dir() {
            roots.push(docs.join("xwechat_files"));
            // 3.x legacy root; listed but not treated as 4.x
            roots.push(docs.join("WeChat Files"));
        }
        roots
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join("Library/Containers/com.tencent.xinWeChat/Data/Library/Application Support/com.tencent.xinWeChat"),
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join("Documents/xwechat_files"),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join("Documents/xwechat_files")]
    }
}

/// A discovered WeChat 4.x account.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    /// Cleaned wxid (e.g. `wxid_abc123` or `wechatid1234`).
    pub wxid: String,
    /// Account directory (`<root>/<wxid>[_[0-9]{4}]`).
    pub dir: PathBuf,
    /// The account's `db_storage` directory.
    pub db_storage: PathBuf,
    /// Preferred session database (`db_storage/session/session.db` or flat).
    pub session_db: Option<PathBuf>,
}

/// An encrypted database file under `db_storage` (with optional WAL sibling).
#[derive(Debug, Clone)]
pub struct DbFile {
    /// Path relative to `db_storage` with `/` separators (matches key maps).
    pub rel: String,
    pub abs: PathBuf,
    /// `-wal` sibling path if present.
    pub wal: Option<PathBuf>,
    /// Coarse classification by relative path.
    pub kind: DbKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Session,
    Message,
    Contact,
    Media,
    Sns,
    Other,
}

/// Strip the `_<4 digits>` suffix WeChat appends to custom ids.
fn clean_wxid(dir_name: &str) -> String {
    if dir_name.len() > 5 && dir_name.as_bytes()[dir_name.len() - 5] == b'_' {
        let tail = &dir_name[dir_name.len() - 4..];
        if tail.bytes().all(|b| b.is_ascii_digit()) {
            return dir_name[..dir_name.len() - 5].to_string();
        }
    }
    dir_name.to_string()
}

fn looks_like_account_dir(dir: &Path) -> bool {
    dir.join("db_storage").is_dir()
        || dir.join("FileStorage").join("Image").is_dir()
        || dir.join("FileStorage").join("Image2").is_dir()
}

/// Scan a data root for accounts. Returns them sorted by wxid.
pub fn scan_root(root: &Path, legacy_3x: bool) -> Vec<AccountInfo> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if !looks_like_account_dir(&p) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        candidates.push((clean_wxid(&name), p));
    }
    // Group by cleaned wxid and pick the best physical dir:
    // prefer one with session.db, then newest mtime, then the suffixed name.
    let mut by_wxid: BTreeSet<String> = candidates.iter().map(|(w, _)| w.clone()).collect();
    by_wxid.retain(|w| {
        let group: Vec<&PathBuf> = candidates
            .iter()
            .filter(|(ww, _)| ww == w)
            .map(|(_, p)| p)
            .collect();
        let mut best: Option<PathBuf> = None;
        let mut best_score = (false, std::time::UNIX_EPOCH, false);
        for p in &group {
            let has_session = find_session_db(p).is_some();
            let mtime = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            let suffixed = p
                .file_name()
                .map(|n| n.to_string_lossy().contains('_'))
                .unwrap_or(false);
            let score = (has_session, mtime, suffixed);
            if score > best_score {
                best_score = score;
                best = Some((*p).clone());
            }
        }
        match best {
            Some(dir) => {
                let db_storage = dir.join("db_storage");
                out.push(AccountInfo {
                    wxid: w.clone(),
                    session_db: find_session_db(&dir),
                    db_storage: if db_storage.is_dir() { db_storage } else { dir.clone() },
                    dir,
                });
                true
            }
            None => false,
        }
    });
    drop(by_wxid);
    let _ = legacy_3x;
    out
}

/// Scan all default roots.
pub fn scan_all(roots: &[PathBuf]) -> Vec<AccountInfo> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        for acc in scan_root(root, false) {
            if seen.insert(acc.wxid.clone()) {
                out.push(acc);
            }
        }
    }
    out
}

/// Find the session database: prefer `db_storage/session/session.db`,
/// fall back to a flat `db_storage/session.db`.
pub fn find_session_db(account_dir: &Path) -> Option<PathBuf> {
    let storage = account_dir.join("db_storage");
    let nested = storage.join("session").join("session.db");
    if nested.is_file() {
        return Some(nested);
    }
    let flat = storage.join("session.db");
    if flat.is_file() {
        return Some(flat);
    }
    None
}

pub fn classify_rel(rel: &str) -> DbKind {
    let lower = rel.to_ascii_lowercase();
    if lower == "migrate" || lower.starts_with("migrate/") {
        return DbKind::Other;
    }
    if lower.starts_with("session") || lower.contains("session.db") {
        DbKind::Session
    } else if lower.starts_with("message") || lower.contains("msg") {
        DbKind::Message
    } else if lower.starts_with("contact") {
        DbKind::Contact
    } else if lower.starts_with("media") {
        DbKind::Media
    } else if lower.starts_with("sns") {
        DbKind::Sns
    } else {
        DbKind::Other
    }
}

/// Enumerate all `.db` files under `db_storage` (recursively), attaching
/// `-wal`/`-shm` siblings. Sorted by relative path for determinism.
pub fn enum_db_files(storage: &Path) -> Vec<DbFile> {
    let mut out = Vec::new();
    if !storage.is_dir() {
        return out;
    }
    fn walk(dir: &Path, storage: &Path, out: &mut Vec<DbFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Skip migration-tool droplet directories entirely.
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "migrate")
                {
                    continue;
                }
                walk(&p, storage, out);
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal") {
                    continue;
                }
                if name.ends_with(".db") {
                    let rel = p
                        .strip_prefix(storage)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let wal = p.with_file_name(format!("{name}-wal"));
                    let kind = classify_rel(&rel);
                    out.push(DbFile {
                        rel,
                        abs: p,
                        wal: if wal.is_file() { Some(wal) } else { None },
                        kind,
                    });
                }
            }
        }
    }
    walk(storage, storage, &mut out);
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wxid_cleaning() {
        assert_eq!(clean_wxid("wxid_abc123"), "wxid_abc123");
        assert_eq!(clean_wxid("wxid_abc123_4567"), "wxid_abc123");
        assert_eq!(clean_wxid("myid_1234"), "myid"); // custom id with suffix
        assert_eq!(clean_wxid("plain_id"), "plain_id");
    }

    #[test]
    fn classification() {
        assert_eq!(classify_rel("session/session.db"), DbKind::Session);
        assert_eq!(classify_rel("message/message_0.db"), DbKind::Message);
        assert_eq!(classify_rel("contact/contact.db"), DbKind::Contact);
        assert_eq!(classify_rel("media_1/media_1.db"), DbKind::Media);
        assert_eq!(classify_rel("emoticon/emoticon.db"), DbKind::Other);
        assert_eq!(classify_rel("migrate/unspportmsg.db"), DbKind::Other);
        assert_eq!(classify_rel("migrate"), DbKind::Other);
    }

    #[test]
    fn enum_skips_wal_shm() {
        let dir = std::env::temp_dir().join(format!("wf-scan-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("session")).unwrap();
        std::fs::create_dir_all(dir.join("message")).unwrap();
        std::fs::write(dir.join("session/session.db"), b"x").unwrap();
        std::fs::write(dir.join("session/session.db-wal"), b"x").unwrap();
        std::fs::write(dir.join("session/session.db-shm"), b"x").unwrap();
        std::fs::write(dir.join("message/message_0.db"), b"x").unwrap();
        std::fs::write(dir.join("message/message_0.db-wal"), b"x").unwrap();
        let files = enum_db_files(&dir);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.wal.is_some()));
        assert!(files.iter().all(|f| !f.rel.contains("-wal")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enum_skips_migrate_dir() {
        let dir = std::env::temp_dir().join(format!("wf-scan-migrate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("migrate")).unwrap();
        std::fs::create_dir_all(dir.join("message")).unwrap();
        std::fs::write(dir.join("migrate/unspportmsg.db"), b"x").unwrap();
        std::fs::write(dir.join("message/message_0.db"), b"x").unwrap();
        let files = enum_db_files(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel, "message/message_0.db");
        let _ = std::fs::remove_dir_all(&dir);
    }
}