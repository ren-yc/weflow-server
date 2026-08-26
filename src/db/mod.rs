//! Database layer.
//!
//! - `scan`: account discovery & file classification
//! - `live`: long-lived read-only SQLCipher connections (qqflow-style live
//!   acquisition — the source of truth for index/poll; no plaintext mirror)
//! - `wcdb`: page-cipher primitives, kept as verification utilities
//!   (`verify_page1` is used by registration pre-validation)
//! - `open`: snapshot/open helpers shared by tests and legacy paths

pub mod live;
pub mod open;
pub mod scan;
pub mod wcdb;
