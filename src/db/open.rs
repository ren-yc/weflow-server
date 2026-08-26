//! Read-only connection/probing helpers over decrypted database content.
//! Everything is defensive: every schema assumption is verified at runtime via
//! `PRAGMA table_info` and degrades gracefully (never panics on drift).

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// Open a decrypted database file read-only with a busy timeout;
/// fails fast on a missing file.
pub fn open_snapshot(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}: {}", "decrypted db", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    Ok(conn)
}

/// Verify the file is a usable SQLite database (at least one table).
pub fn quick_check(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table'",
        [],
        |r| r.get(0),
    )?;
    if count == 0 {
        bail!("snapshot has no tables (decryption produced an unusable db)");
    }
    Ok(())
}

/// List the columns of a table (empty if the table is missing).
pub fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    
    stmt
        .query_map([], |r| r.get::<_, String>(1))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
        .unwrap_or_default()
}

/// List table names (first `limit`).
pub fn table_names(conn: &Connection, limit: usize) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([limit as i64], |r| r.get::<_, String>(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn probe_helpers_handle_missing_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER, b TEXT);").unwrap();
        assert_eq!(table_columns(&conn, "t"), vec!["a".to_string(), "b".to_string()]);
        assert!(table_columns(&conn, "nope").is_empty());
        assert_eq!(table_names(&conn, 10), vec!["t".to_string()]);
        assert!(quick_check(&conn).is_ok());
    }
}