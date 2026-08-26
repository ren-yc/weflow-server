//! Format diagnostic against a REAL WeChat 4.0 database (copy).
//!
//! Brute-forces the *layout* parameters that do not need the MAC key
//! (reserve size, salt offset, IV placement, AES-128/256) validated by the
//! decrypted "SQLite format 3\0" magic, over several real db files.

use std::env;
use std::fs;
use std::sync::OnceLock;

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};

fn try_decrypt_aes256(key: &[u8; 32], iv: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    if ct.len() % 16 != 0 || iv.len() != 16 {
        return None;
    }
    let dec = cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into());
    let mut buf = ct.to_vec();
    dec.decrypt_padded_mut::<NoPadding>(&mut buf).ok()?;
    Some(buf)
}

fn try_decrypt_aes128(key: &[u8; 16], iv: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    if ct.len() % 16 != 0 || iv.len() != 16 {
        return None;
    }
    let dec = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into());
    let mut buf = ct.to_vec();
    dec.decrypt_padded_mut::<NoPadding>(&mut buf).ok()?;
    Some(buf)
}

fn probe_one(db_path: &str, key_full: &[u8; 32]) -> usize {
    let Ok(db) = fs::read(db_path) else {
        println!("unreadable {db_path}");
        return 0;
    };
    const PAGE: usize = 4096;
    if db.len() < PAGE {
        println!("{db_path}: too small");
        return 0;
    }
    let page = &db[..PAGE];
    let key128: [u8; 16] = key_full[..16].try_into().unwrap();
    let mut found = 0;
    for reserve in [16usize, 20, 24, 28, 36, 44, 48, 52, 64, 80, 96, 112] {
        for salt_offset in [0usize, 4, 8, 16, 20, 32] {
            let iv_end = PAGE - reserve;
            if iv_end < salt_offset + 16 {
                continue;
            }
            let ct_start = salt_offset;
            let ct_end = iv_end;
            if ct_start >= ct_end || (ct_end - ct_start) % 16 != 0 {
                continue;
            }
            let iv = &page[iv_end..iv_end + 16];
            let ct = &page[ct_start..ct_end];
            if let Some(pt) = try_decrypt_aes256(key_full, iv, ct) {
                if pt.starts_with(b"SQLite format 3\0") {
                    found += 1;
                    println!(
                        "FOUND256 [{}]: reserve={reserve} salt_offset={salt_offset} iv@end pgsz={} hdr20={}",
                        db_path,
                        u16::from_be_bytes([pt[16], pt[17]]),
                        pt[20]
                    );
                }
            }
            let iv2_start = salt_offset + 16;
            if iv2_start + 16 <= iv_end {
                let iv2 = &page[salt_offset..iv2_start];
                let ct2 = &page[iv2_start..iv_end];
                if let Some(pt) = try_decrypt_aes256(key_full, iv2, ct2) {
                    if pt.starts_with(b"SQLite format 3\0") {
                        found += 1;
                        println!(
                            "FOUND256 [{}]: reserve={reserve} salt_offset={salt_offset} iv@start",
                            db_path
                        );
                    }
                }
            }
            let iv3 = &page[iv_end..iv_end + 16];
            let ct3 = &page[ct_start..ct_end];
            if let Some(pt) = try_decrypt_aes128(&key128, iv3, ct3) {
                if pt.starts_with(b"SQLite format 3\0") {
                    found += 1;
                    println!(
                        "FOUND128 [{}]: reserve={reserve} salt_offset={salt_offset} iv@end",
                        db_path
                    );
                }
            }
        }
    }
    found
}

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

#[test]
#[ignore = "requires a real WeChat 4.0 account (WEFLOW_TEST_DB_ROOT/WEFLOW_TEST_KEY)"]
fn probe() {
    let Some(root) = env::var("WEFLOW_TEST_DB_ROOT").ok() else {
        println!("env WEFLOW_TEST_DB_ROOT required");
        return;
    };
    let Some(key_hex) = env::var("WEFLOW_TEST_KEY").ok() else {
        println!("env WEFLOW_TEST_KEY required");
        return;
    };
    let key_full: [u8; 32] = hex::decode(&key_hex)
        .unwrap_or_default()
        .try_into()
        .expect("64-hex key");
    let base = format!("{}/db_storage", root.trim_end_matches('/'));
    for rel in [
        "session/session.db",
        "message/message_0.db",
        "contact/contact.db",
        "message/message_1.db",
        "sns/sns.db",
    ] {
        let path = format!("{base}/{rel}");
        let n = probe_one(&path, &key_full);
        println!("probe {rel}: {n} layout hits");
    }
}