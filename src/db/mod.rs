//! Database layer: scan (account discovery), wcdb (page cipher), mirror
//! (decrypted snapshot), open (read the snapshot with rusqlite).

pub mod mirror;
pub mod open;
pub mod scan;
pub mod wcdb;