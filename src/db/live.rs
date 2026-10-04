//! Long-lived read-only live connections to WeChat's encrypted databases —
//! the qqflow-style acquisition layer (no mirror, no plaintext on disk).
//!
//! Each database file gets one connection that stays open across polls
//! (warm page cache, no repeated key derivation). WeChat 4.x `.db` files are
//! standard SQLCipher layouts, so no custom VFS is needed: the raw 32-byte
//! enc_key is supplied via the raw-key form `PRAGMA key = "x'<hex>'"`, which
//! skips KDF entirely and matches the page-cipher math verified in
//! `db::wcdb`.
//!
//! Concurrency contract with the live WeChat client:
//! - connections are READ_ONLY + `query_only` (we can never write)
//! - WAL lets readers run while WeChat writes; we never hold transactions
//!   across polls, so checkpoints are never blocked by us

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::scan::DbFile;
use crate::keystore::DbKey;

/// Why a live connection could not be acquired.
#[derive(Debug)]
pub enum AcquireError {
    /// Page-1 HMAC failed inside SQLite (`file is not a database` family).
    WrongKey,
    /// The source file disappeared (account moved / pruned).
    Missing,
    /// Anything else (locked beyond timeout, I/O error, …).
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::WrongKey => write!(f, "key failed verification (wrong key for this db)"),
            AcquireError::Missing => write!(f, "source file missing"),
            AcquireError::Io(e) => write!(f, "open failed: {e}"),
        }
    }
}

fn io_err(e: &rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn map_open_error(e: &rusqlite::Error, _path: &Path) -> AcquireError {
    let msg = e.to_string();
    if msg.contains("file is not a database")
        || msg.contains("not a database")
        || msg.contains("HMAC")
        || msg.to_lowercase().contains("key")
    {
        AcquireError::WrongKey
    } else if msg.contains("unable to open database file") {
        AcquireError::Missing
    } else {
        AcquireError::Io(std::io::Error::other(msg))
    }
}

/// Open a fresh read-only SQLCipher connection with the raw key
/// (`x'<hex>'` form — no KDF). Used for one-shot auxiliary lookups.
pub fn open_read_only(path: &Path, key_hex64: &str) -> Result<Connection, AcquireError> {
    if !path.is_file() {
        return Err(AcquireError::Missing);
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| AcquireError::Io(io_err(&e)))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|e| AcquireError::Io(io_err(&e)))?;
    let pragmas = [
        format!("PRAGMA key = \"x'{key_hex64}'\";"),
        "PRAGMA query_only = ON;".into(),
    ];
    for p in &pragmas {
        conn.execute_batch(p)
            .map_err(|e| map_open_error(&e, path))?;
    }
    // force page-1 decryption now (fails here, not mid-query later)
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|e| map_open_error(&e, path))?;
    Ok(conn)
}

/// One long-lived read-only connection to a single database file.
pub struct LiveDb {
    #[allow(dead_code)]
    rel: String,
    path: PathBuf,
    key_hex64: String,
    conn: Option<Connection>,
    /// `PRAGMA data_version` observed on the current connection right after
    /// open, and refreshed whenever the caller asks. It is a **connection-local
    ///** property: comparing values is only meaningful between two points in
    /// time on the *same* connection, so the baseline dies with the connection
    /// (`None` whenever `conn` is `None`). `None` also means "no baseline —
    /// treat the file as changed".
    data_version: Option<i64>,
}

impl LiveDb {
    pub fn new(rel: impl Into<String>, path: PathBuf, key_hex64: impl Into<String>) -> Self {
        Self { rel: rel.into(), path, key_hex64: key_hex64.into(), conn: None, data_version: None }
    }

    pub fn is_open(&self) -> bool {
        self.conn.is_some()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_key_hex64(&mut self, hex64: impl Into<String>) {
        self.key_hex64 = hex64.into();
    }

    pub fn stored_key(&self) -> &str {
        &self.key_hex64
    }

    /// Open (or reopen) the connection. Verifies the key eagerly by touching
    /// sqlite_master so wrong-key failures surface as `WrongKey`.
    pub fn open(&mut self) -> Result<(), AcquireError> {
        let conn = open_read_only(&self.path, &self.key_hex64)?;
        self.data_version = Self::read_data_version(&conn).ok();
        self.conn = Some(conn);
        Ok(())
    }

    pub fn close(&mut self) {
        // The baseline is connection-local: with the connection gone it is
        // meaningless, and "no baseline" must read as "changed".
        self.conn = None;
        self.data_version = None;
    }

    /// Connection, reopening transparently when previously closed.
    pub fn acquire(&mut self) -> Result<&Connection, AcquireError> {
        if self.conn.is_none() {
            self.open()?;
        }
        Ok(self.conn.as_ref().expect("just opened"))
    }

    fn read_data_version(conn: &Connection) -> rusqlite::Result<i64> {
        conn.query_row("PRAGMA data_version", [], |r| r.get(0))
    }

}

/// Pool of live connections keyed by relative path.
///
/// Connections are opened lazily; a `WrongKey` failure is remembered so the
/// poll layer stops hammering databases whose key we do not have.
#[derive(Default)]
pub struct LivePool {
    pool: HashMap<String, LiveDb>,
    keyless: std::collections::HashSet<String>,
}

impl LivePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a rel as known-keyless (poll layer stops retrying it).
    pub fn mark_keyless(&mut self, rel: &str) {
        self.keyless.insert(rel.to_string());
    }

    pub fn is_keyless(&self, rel: &str) -> bool {
        self.keyless.contains(rel)
    }

    /// Get or lazily open the connection for `f`. Reopens after close/errors.
    ///
    /// A previously-marked keyless rel is retried exactly once per call so a
    /// corrected registration can recover without a restart.
    pub fn get_or_open(
        &mut self,
        f: &DbFile,
        key: DbKey,
    ) -> Result<&Connection, AcquireError> {
        let incoming = hex::encode(key.0);
        // Backoff for deterministic failures (wrong key): fail fast without
        // touching the file — but only while the registered key is unchanged;
        // a corrected registration clears the mark and opens normally.
        if self.is_keyless(&f.rel) {
            let unchanged = self
                .pool
                .get(&f.rel)
                .is_none_or(|e| !e.is_open() && e.stored_key() == incoming);
            if unchanged {
                return Err(AcquireError::WrongKey);
            }
            self.keyless.remove(&f.rel);
        }
        let entry = self.pool.entry(f.rel.clone()).or_insert_with(|| {
            LiveDb::new(f.rel.clone(), f.abs.clone(), incoming.clone())
        });
        // keep the stored raw key in sync if registration changed it
        entry.set_key_hex64(incoming);
        entry.acquire()
    }

    /// Is there an **open** connection for this rel? The sync layer uses this
    /// to pick the change-detection path: open files probe `data_version`,
    /// the rest fall back to mtime/size stamps.
    pub fn is_open(&self, rel: &str) -> bool {
        self.pool.get(rel).is_some_and(LiveDb::is_open)
    }

    /// Close (and drop the data_version baseline of) one entry, e.g. when the
    /// caller decides a failed round must not keep a stale handle.
    pub fn close(&mut self, rel: &str) {
        if let Some(entry) = self.pool.get_mut(rel) {
            entry.close();
        }
    }

    /// Has any *other* connection committed to this file since we last
    /// looked, **without advancing the baseline**? `None` when there is no
    /// open connection. The caller advances the baseline only after it
    /// actually consumed the file's data — a round that classifies the file
    /// as changed but then fails to read it must leave the baseline alone.
    pub fn foreign_commit_pending(&self, rel: &str) -> Option<bool> {
        let entry = self.pool.get(rel)?;
        let conn = entry.conn.as_ref()?;
        let now = LiveDb::read_data_version(conn).ok()?;
        Some(entry.data_version != Some(now))
    }

    /// Consume the pending change state: re-baseline `data_version` to the
    /// current value. Called after a successful incremental read.
    pub fn advance_data_version(&mut self, rel: &str) {
        if let Some(entry) = self.pool.get_mut(rel)
            && let Some(conn) = entry.conn.as_ref()
            && let Ok(now) = LiveDb::read_data_version(conn)
        {
            entry.data_version = Some(now);
        }
    }
}
