//! WeChat 4.0 database page cipher (SQLCipher-4 derived, as confirmed by
//! wechat-decrypt's live implementation + Kanxue/cn-sec + DeepWiki analyses).
//!
//! Format facts:
//! - page size 4096, reserve = 80 (IV 16 + HMAC-SHA512 64), usable = 4016
//! - page 1: `[salt 16][ciphertext 4000][iv 16 @4016][hmac 64 @4032]`
//! - pages > 1: `[ciphertext 4016][iv 16 @4016][hmac 64 @4032]` (no salt)
//! - AES-256-CBC, no padding; the IV is stored inside the page (no derivation)
//! - mac_key = PBKDF2-HMAC-SHA512(enc_key, salt ^ 0x3a bytewise, 2 rounds, 32B)
//! - HMAC input = ciphertext || iv || LE u32(pgno)
//! - decrypted page 1 = "SQLite format 3\0" + plaintext + 80 zero bytes
//! - every database has its own 32-byte enc_key and 16-byte salt; a key is
//!   validated deterministically by recomputing the page-1 HMAC
//! - WAL files are preallocated to a fixed 4 MB; frames are
//!   `[24B header][4096B encrypted page]`; frame salt must match the WAL
//!   header salt (stale frames from previous cycles are skipped)

use anyhow::{bail, Result};
use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use aes::Aes256;
use cbc::{Decryptor, Encryptor};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha512;

pub const PAGE_SIZE: usize = 4096;
pub const RESERVE_SIZE: usize = 80; // IV(16) + HMAC(64)
pub const IV_SIZE: usize = 16;
pub const HMAC_SIZE: usize = 64;
pub const SALT_SIZE: usize = 16;
pub const USABLE_SIZE: usize = PAGE_SIZE - RESERVE_SIZE; // 4016
/// Standard SQLite header magic, re-attached when decrypting page 1.
pub const SQLITE_HDR: &[u8; 16] = b"SQLite format 3\0";

// WAL layout
pub const WAL_HEADER_SIZE: usize = 32;
pub const WAL_FRAME_HEADER_SIZE: usize = 24;
pub const WAL_FRAME_SIZE: usize = WAL_FRAME_HEADER_SIZE + PAGE_SIZE; // 4120
/// WAL files are preallocated to this fixed size; size alone cannot detect
/// changes, only mtime/last-write.
pub const WAL_PREALLOCATED_SIZE: usize = 4 * 1024 * 1024;

pub type Key = [u8; 32];

/// Derive the HMAC key: PBKDF2-HMAC-SHA512(enc_key, salt ^ 0x3a, 2, 32).
pub fn derive_mac_key(enc_key: &Key, salt: &[u8; SALT_SIZE]) -> Key {
    let mac_salt: Vec<u8> = salt.iter().map(|b| b ^ 0x3a).collect();
    let mut mac_key = [0u8; 32];
    pbkdf2_hmac::<Sha512>(enc_key, &mac_salt, 2, &mut mac_key);
    mac_key
}

/// Compute the HMAC-SHA512 over `ciphertext || iv || LE32(pgno)`.
fn page_mac(mac_key: &Key, ct_iv: &[u8], pgno: u32) -> [u8; HMAC_SIZE] {
    let mut mac = Hmac::<Sha512>::new_from_slice(mac_key).expect("hmac accepts any key");
    mac.update(ct_iv);
    mac.update(&pgno.to_le_bytes());
    mac.finalize().into_bytes().into()
}

fn aes_cbc_decrypt(key: &Key, iv: &[u8; IV_SIZE], ct: &[u8]) -> Result<Vec<u8>> {
    let dec = Decryptor::<Aes256>::new(key.into(), iv.into());
    let mut buf = ct.to_vec();
    dec.decrypt_padded_mut::<NoPadding>(&mut buf)
        .map_err(|_| anyhow::anyhow!("AES-CBC decrypt failed"))?;
    Ok(buf)
}

fn aes_cbc_encrypt(key: &Key, iv: &[u8; IV_SIZE], pt: &[u8]) -> Vec<u8> {
    let enc = Encryptor::<Aes256>::new(key.into(), iv.into());
    let mut buf = pt.to_vec();
    let len = buf.len();
    enc.encrypt_padded_mut::<NoPadding>(&mut buf, len)
        .expect("plaintext length is a multiple of 16");
    buf
}

/// Verify page 1 of an encrypted database against a candidate key.
/// Deterministic: a wrong key always fails, a right key always passes.
pub fn verify_page1(enc_key: &Key, page: &[u8]) -> bool {
    if page.len() < PAGE_SIZE {
        return false;
    }
    let mut salt = [0u8; SALT_SIZE];
    salt.copy_from_slice(&page[..SALT_SIZE]);
    let mac_key = derive_mac_key(enc_key, &salt);
    // HMAC input for page 1 excludes the salt: ciphertext(16..4016) + iv(4016..4032)
    let expected = page_mac(&mac_key, &page[16..USABLE_SIZE + IV_SIZE], 1);
    expected == page[PAGE_SIZE - HMAC_SIZE..]
}

/// Decrypt a single 4096-byte page into the plaintext SQLite layout.
pub fn decrypt_page(enc_key: &Key, page: &[u8], pgno: u32) -> Result<[u8; PAGE_SIZE]> {
    if page.len() < PAGE_SIZE {
        bail!("page {pgno} truncated: {} bytes", page.len());
    }
    let mut iv = [0u8; IV_SIZE];
    iv.copy_from_slice(&page[USABLE_SIZE..USABLE_SIZE + IV_SIZE]);
    let ct = if pgno == 1 {
        &page[SALT_SIZE..USABLE_SIZE]
    } else {
        &page[..USABLE_SIZE]
    };
    let pt = aes_cbc_decrypt(enc_key, &iv, ct)?;

    let mut out = [0u8; PAGE_SIZE];
    if pgno == 1 {
        out[..16].copy_from_slice(SQLITE_HDR);
        out[16..16 + pt.len()].copy_from_slice(&pt);
    } else {
        out[..pt.len()].copy_from_slice(&pt);
    }
    Ok(out)
}

/// Decrypt a WAL-frame page: always a full-page ciphertext (no salt) even for
/// pgno 1 — SQLCipher's WAL does not carry the salt anywhere.
pub fn decrypt_page_raw(enc_key: &Key, page: &[u8]) -> Result<[u8; PAGE_SIZE]> {
    if page.len() < PAGE_SIZE {
        bail!("wal frame page truncated: {} bytes", page.len());
    }
    let mut iv = [0u8; IV_SIZE];
    iv.copy_from_slice(&page[USABLE_SIZE..USABLE_SIZE + IV_SIZE]);
    let pt = aes_cbc_decrypt(enc_key, &iv, &page[..USABLE_SIZE])?;
    let mut out = [0u8; PAGE_SIZE];
    out[..pt.len()].copy_from_slice(&pt);
    Ok(out)
}

/// Encrypt a plaintext page into the WeChat 4.0 on-disk layout (used by the
/// test fixture generator and roundtrip arbitration; `rng` supplies IVs).
pub fn encrypt_page(
    enc_key: &Key,
    salt: &[u8; SALT_SIZE],
    pgno: u32,
    plain: &[u8; PAGE_SIZE],
    rng: &mut impl RngCore,
) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    if pgno == 1 {
        page[..SALT_SIZE].copy_from_slice(salt);
    }
    let mut iv = [0u8; IV_SIZE];
    rng.fill_bytes(&mut iv);
    let src = if pgno == 1 {
        &plain[SALT_SIZE..USABLE_SIZE]
    } else {
        &plain[..USABLE_SIZE]
    };
    let ct = aes_cbc_encrypt(enc_key, &iv, src);
    let dst = if pgno == 1 {
        &mut page[SALT_SIZE..USABLE_SIZE]
    } else {
        &mut page[..USABLE_SIZE]
    };
    dst.copy_from_slice(&ct);
    page[USABLE_SIZE..USABLE_SIZE + IV_SIZE].copy_from_slice(&iv);
    let mac_key = derive_mac_key(enc_key, salt);
    let hmac_input = if pgno == 1 {
        &page[16..USABLE_SIZE + IV_SIZE]
    } else {
        &page[..USABLE_SIZE + IV_SIZE]
    };
    let mac = page_mac(&mac_key, hmac_input, pgno);
    page[PAGE_SIZE - HMAC_SIZE..].copy_from_slice(&mac);
    page
}

/// Encrypt a full page with no salt handling (WAL-frame layout, incl. pgno 1).
/// Test-only symmetry for `decrypt_page_raw`.
pub fn encrypt_page_raw(
    enc_key: &Key,
    pgno: u32,
    plain: &[u8; PAGE_SIZE],
    rng: &mut impl RngCore,
) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    let mut iv = [0u8; IV_SIZE];
    rng.fill_bytes(&mut iv);
    let ct = aes_cbc_encrypt(enc_key, &iv, &plain[..USABLE_SIZE]);
    page[..USABLE_SIZE].copy_from_slice(&ct);
    page[USABLE_SIZE..USABLE_SIZE + IV_SIZE].copy_from_slice(&iv);
    let mac_key = derive_mac_key(enc_key, &[0u8; SALT_SIZE]);
    let mac = page_mac(&mac_key, &page[..USABLE_SIZE + IV_SIZE], pgno);
    page[PAGE_SIZE - HMAC_SIZE..].copy_from_slice(&mac);
    page
}

/// Decrypt a whole encrypted database file into plaintext SQLite bytes.
/// Verifies page 1 first (wrong key fails here).
pub fn decrypt_db(enc_key: &Key, encrypted: &[u8]) -> Result<Vec<u8>> {
    if encrypted.len() < PAGE_SIZE {
        bail!("db file too small ({} bytes)", encrypted.len());
    }
    if !verify_page1(enc_key, &encrypted[..PAGE_SIZE]) {
        bail!("page 1 HMAC verification failed (wrong key or not a WeChat 4.0 db)");
    }
    let total_pages = encrypted.len().div_ceil(PAGE_SIZE);
    let mut out = Vec::with_capacity(total_pages * PAGE_SIZE);
    let mut page_buf = [0u8; PAGE_SIZE];
    for pgno in 1..=total_pages as u32 {
        let start = (pgno as usize - 1) * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(encrypted.len());
        page_buf[..end - start].copy_from_slice(&encrypted[start..end]);
        if end - start < PAGE_SIZE {
            page_buf[end - start..].fill(0);
        }
        let dec = decrypt_page(enc_key, &page_buf, pgno)?;
        out.extend_from_slice(&dec);
    }
    Ok(out)
}

/// A valid WAL frame (already decrypted), ready to be patched into a snapshot.
pub struct WalFrame {
    pub pgno: u32,
    pub page: [u8; PAGE_SIZE],
}

/// Parse and decrypt the currently-valid frames of an encrypted WAL file.
///
/// Frames whose salt does not match the WAL header salt belong to a previous
/// cycle and are skipped (the WAL is preallocated and never shrinks).
pub fn decrypt_wal_frames(enc_key: &Key, wal: &[u8]) -> Vec<WalFrame> {
    let mut frames = Vec::new();
    if wal.len() <= WAL_HEADER_SIZE {
        return frames;
    }
    let salt1 = u32::from_be_bytes(wal[16..20].try_into().unwrap());
    let salt2 = u32::from_be_bytes(wal[20..24].try_into().unwrap());
    let mut off = WAL_HEADER_SIZE;
    while off + WAL_FRAME_SIZE <= wal.len() {
        let hdr = &wal[off..off + WAL_FRAME_HEADER_SIZE];
        let pgno = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        let frame_salt1 = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        let frame_salt2 = u32::from_be_bytes(hdr[12..16].try_into().unwrap());
        if pgno != 0 && pgno <= 1_000_000 && frame_salt1 == salt1 && frame_salt2 == salt2 {
            // WAL frame pages follow the DB page layout: pgno-1 frames embed
            // the salt at [0..16), others are full-page ciphertext.
            if let Ok(page) = decrypt_page(enc_key, &wal[off + 24..off + WAL_FRAME_SIZE], pgno) {
                frames.push(WalFrame { pgno, page });
            }
        }
        off += WAL_FRAME_SIZE;
    }
    frames
}

/// Apply valid WAL frames onto a decrypted snapshot at `(pgno-1)*4096`.
/// The snapshot grows as needed: WAL pages may point beyond the pages
/// currently in the main file (the main file's own header may lag behind).
/// Later frames override earlier ones (frames are ordered in the WAL).
pub fn apply_wal_frames(snapshot: &mut Vec<u8>, frames: &[WalFrame]) {
    for f in frames {
        let off = (f.pgno as usize - 1) * PAGE_SIZE;
        if off + PAGE_SIZE > snapshot.len() {
            snapshot.resize(off + PAGE_SIZE, 0);
        }
        snapshot[off..off + PAGE_SIZE].copy_from_slice(&f.page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::thread_rng;

    fn key_from(seed: u8) -> Key {
        [seed; 32]
    }

    #[test]
    fn page1_roundtrip_and_hmac() {
        let mut rng = thread_rng();
        let key = key_from(7);
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let mut plain = [0u8; PAGE_SIZE];
        plain[..16].copy_from_slice(SQLITE_HDR);
        plain[20] = 80; // reserved byte
        plain[100] = 0xAB;
        plain[4000] = 0xCD;

        let enc = encrypt_page(&key, &salt, 1, &plain, &mut rng);
        assert!(verify_page1(&key, &enc), "correct key must verify");
        assert!(
            !verify_page1(&key_from(8), &enc),
            "wrong key must fail page-1 HMAC"
        );
        let dec = decrypt_page(&key, &enc, 1).unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn page2_roundtrip() {
        let mut rng = thread_rng();
        let key = key_from(3);
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let mut plain = [0u8; PAGE_SIZE];
        plain[64..80].copy_from_slice(b"hello page two!!");
        let enc = encrypt_page(&key, &salt, 2, &plain, &mut rng);
        let dec = decrypt_page(&key, &enc, 2).unwrap();
        assert_eq!(dec, plain);
        // ciphertext must actually differ from plaintext
        assert_ne!(&enc[..16], &plain[..16]);
    }

    #[test]
    fn truncated_page_and_file_rejected() {
        let key = key_from(1);
        assert!(!verify_page1(&key, &[0u8; 100]));
        assert!(decrypt_page(&key, &[0u8; PAGE_SIZE - 1], 1).is_err());
        assert!(decrypt_db(&key, &[0u8; 100]).is_err());
    }

    #[test]
    fn whole_file_roundtrip() {
        let mut rng = thread_rng();
        let key = key_from(9);
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let pages: usize = 3;
        let mut plain_file = Vec::new();
        for pgno in 1..=(pages as u32) {
            let mut p = [0u8; PAGE_SIZE];
            if pgno == 1 {
                p[..16].copy_from_slice(SQLITE_HDR);
                p[20] = 80;
                p[24..32].copy_from_slice(b"encrpt01");
            } else {
                p[0..8].copy_from_slice(&(pgno as u64).to_be_bytes());
            }
            plain_file.extend_from_slice(&encrypt_page(&key, &salt, pgno, &p, &mut rng));
        }
        let dec = decrypt_db(&key, &plain_file).unwrap();
        assert_eq!(dec.len(), pages * PAGE_SIZE);
        assert_eq!(&dec[..16], SQLITE_HDR);
        assert_eq!(dec[20], 80);
        assert_eq!(&dec[24..32], b"encrpt01");
        assert_eq!(dec[PAGE_SIZE..PAGE_SIZE + 8], 2u64.to_be_bytes());
        assert!(decrypt_db(&key_from(0), &plain_file).is_err());
    }

    #[test]
    fn wal_frames_parsed_and_applied() {
        let mut rng = thread_rng();
        let key = key_from(11);
        let mut salt = [0u8; 16];
        rng.fill_bytes(&mut salt);
        let salt1 = u32::from_be_bytes(salt[0..4].try_into().unwrap());
        let salt2 = u32::from_be_bytes(salt[4..8].try_into().unwrap());

        let mut wal = vec![0u8; WAL_HEADER_SIZE];
        wal[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
        wal[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_be_bytes());
        wal[16..20].copy_from_slice(&salt1.to_be_bytes());
        wal[20..24].copy_from_slice(&salt2.to_be_bytes());

        // frame for page 2
        let mut plain = [0u8; PAGE_SIZE];
        plain[0..16].copy_from_slice(b"fresh wal conten");
        let enc_page = encrypt_page(&key, &salt, 2, &plain, &mut rng);
        let mut frame = vec![0u8; WAL_FRAME_SIZE];
        frame[0..4].copy_from_slice(&2u32.to_be_bytes());
        frame[8..12].copy_from_slice(&salt1.to_be_bytes());
        frame[12..16].copy_from_slice(&salt2.to_be_bytes());
        frame[24..].copy_from_slice(&enc_page);
        // frame for page 1: DB-style layout (salt at [0..16)) — WAL embeds salt for pgno 1
        let mut plain1 = [0u8; PAGE_SIZE];
        plain1[..16].copy_from_slice(b"SQLite format 3\0");
        plain1[20] = 80;
        plain1[100..116].copy_from_slice(b"wal page one!!!!");
        let enc_page1 = encrypt_page(&key, &salt, 1, &plain1, &mut rng);
        let mut frame1 = vec![0u8; WAL_FRAME_SIZE];
        frame1[0..4].copy_from_slice(&1u32.to_be_bytes());
        frame1[8..12].copy_from_slice(&salt1.to_be_bytes());
        frame1[12..16].copy_from_slice(&salt2.to_be_bytes());
        frame1[24..].copy_from_slice(&enc_page1);
        // stale frame for page 3 with mismatched salt must be skipped
        let mut stale = vec![0u8; WAL_FRAME_SIZE];
        stale[0..4].copy_from_slice(&3u32.to_be_bytes());
        stale[8..12].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        stale[12..16].copy_from_slice(&0xCAFE_F00Du32.to_be_bytes());
        stale[24..].copy_from_slice(&enc_page);

        wal.extend_from_slice(&frame1);
        wal.extend_from_slice(&frame);
        wal.extend_from_slice(&stale);

        let frames = decrypt_wal_frames(&key, &wal);
        assert_eq!(frames.len(), 2, "stale frame must be skipped; pgno1+2 kept");
        let f1 = frames.iter().find(|f| f.pgno == 1).unwrap();
        assert_eq!(&f1.page[..16], b"SQLite format 3\0");
        assert_eq!(f1.page[20], 80);
        assert_eq!(&f1.page[100..116], b"wal page one!!!!");
        let f2 = frames.iter().find(|f| f.pgno == 2).unwrap();
        assert_eq!(&f2.page[..16], b"fresh wal conten");

        // apply onto a 3-page snapshot
        let mut snapshot = vec![0u8; 3 * PAGE_SIZE];
        snapshot[2 * PAGE_SIZE..2 * PAGE_SIZE + 16].copy_from_slice(b"old content     ");
        apply_wal_frames(&mut snapshot, &frames);
        assert_eq!(&snapshot[..16], b"SQLite format 3\0");
        assert_eq!(&snapshot[PAGE_SIZE..PAGE_SIZE + 16], b"fresh wal conten");
        assert_eq!(&snapshot[2 * PAGE_SIZE..2 * PAGE_SIZE + 16], b"old content     ");
    }
}