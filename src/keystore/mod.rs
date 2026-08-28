//! Database key management.
//!
//! WeChat 4.x keeps an independent 32-byte (64-hex) `enc_key` per database
//! file, together with a per-file 16-byte salt. Keys are validated
//! deterministically via the page-1 HMAC (see `db::wcdb::verify_page1`), so a
//! wrong key can never be accepted. Keys are kept in memory only and are never
//! written to disk.

use std::collections::HashMap;

/// A validated 32-byte WeChat 4.x database encryption key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbKey(pub [u8; 32]);

impl From<[u8; 32]> for DbKey {
    fn from(bytes: [u8; 32]) -> Self {
        DbKey(bytes)
    }
}

/// Per-database key resolution: WeChat 4.x keeps an independent `enc_key` per
/// database file (`all_keys.json` style: relative path -> 64-hex key), while
/// some accounts accept a single uniform key. `KeyMap` supports both and is
/// the runtime key source for the live/sync layers.
#[derive(Debug, Clone, Default)]
pub enum KeyMap {
    #[default]
    /// No key registered (nothing decrypts).
    Empty,
    /// One key applied to every database.
    Single(DbKey),
    /// Relative path (`/` separators) -> key.
    Map(HashMap<String, DbKey>),
}

impl From<DbKey> for KeyMap {
    fn from(k: DbKey) -> Self {
        KeyMap::Single(k)
    }
}

impl KeyMap {
    /// Resolve the key for one db file (relative path with `/` separators).
    pub fn key_for(&self, rel: &str) -> Option<DbKey> {
        match self {
            KeyMap::Empty => None,
            KeyMap::Single(k) => Some(*k),
            KeyMap::Map(m) => m.get(rel).copied().or_else(|| {
                // tolerate backslash-style lookup keys
                let bs = rel.replace('/', "\\");
                m.get(&bs).copied()
            }),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, KeyMap::Empty)
    }

    /// Parse a registration payload: `key` (uniform) and/or `keys` (per-db map).
    pub fn from_parts(
        key: Option<DbKey>,
        keys: Option<HashMap<String, String>>,
    ) -> anyhow::Result<KeyMap> {
        if let Some(k) = key {
            return Ok(KeyMap::Single(k));
        }
        let Some(map) = keys else {
            return Ok(KeyMap::Empty);
        };
        let mut m = HashMap::new();
        for (rel, hex) in map {
            m.insert(rel, parse_db_key(&hex)?);
        }
        Ok(KeyMap::Map(m))
    }
}

/// Parse a 64-hex-character (32-byte) key. Rejects anything else.
pub fn parse_db_key(raw: &str) -> anyhow::Result<DbKey> {
    let s = raw.trim();
    if s.len() != 64 {
        anyhow::bail!(
            "invalid db key: expected 64 hex chars (32 bytes), got {} chars",
            s.len()
        );
    }
    let bytes = hex::decode(s)
        .map_err(|_| anyhow::anyhow!("invalid db key: not valid hex"))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(DbKey(out))
}

/// An optional image-decryption code supplied at registration time
/// (`img_code`). Derives `xorKey = code & 0xff` and
/// `aesKey = MD5(code + wxid)[..16]` for `.dat` V2 files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgCode(pub String);

impl ImgCode {
    pub fn xor_key(&self) -> u8 {
        // WeFlow/wechat-decrypt treat the code as a byte string; the low byte
        // feed is the bytewise-xored value used by .dat V2/V3.
        match self.0.as_bytes().first() {
            Some(b) => *b,
            None => 0,
        }
    }

    pub fn aes_key_hex(&self, wxid: &str) -> String {
        use md5::Digest;
        let mut m = md5::Md5::new();
        m.update(self.0.as_bytes());
        m.update(wxid.as_bytes());
        let digest = m.finalize();
        hex::encode(&digest[..8]) // first 16 hex chars = 8 bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_key_shapes() {
        let ok = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(parse_db_key(ok).is_ok());
        assert!(parse_db_key("").is_err());
        assert!(parse_db_key("0123456789abcdef").is_err()); // 32 hex = 16 bytes is the 3.x shape
        assert!(parse_db_key(&format!("{ok}zz")).is_err()); // non-hex
    }

    #[test]
    fn img_code_derivation_matches_weflow() {
        let c = ImgCode("0".to_string());
        // code "0" -> xor key 0x30 (ASCII '0'), aes hex = md5("0"+wxid)[..16]
        assert_eq!(c.xor_key(), 0x30);
        let aes = c.aes_key_hex("wxid_test");
        assert_eq!(aes.len(), 16);
    }
}
/// Precomputed WeChat image-decryption keys (`.dat` V2 / legacy XOR).
///
/// Either derived from a registered `img_code` at registration time or passed
/// directly as `img_aes_key` (16 ASCII chars) + `img_xor_key`.
#[derive(Debug, Clone, Copy)]
pub struct ImageKeys {
    pub aes: [u8; 16],
    pub xor: u8,
}

impl ImageKeys {
    pub fn from_img_code(code: &ImgCode, wxid: &str) -> Self {
        let hex = code.aes_key_hex(wxid);
        let mut aes = [0u8; 16];
        aes.copy_from_slice(hex.as_bytes());
        Self { aes, xor: code.xor_key() }
    }

    /// `aes_hex`: exactly 16 characters; `xor`: accepts "0x64" or "100".
    pub fn from_parts(aes_hex: &str, xor: &str) -> anyhow::Result<Self> {
        let b = aes_hex.as_bytes();
        if b.len() != 16 {
            anyhow::bail!("img_aes_key must be 16 characters");
        }
        let xor_v = if let Some(h) = xor.strip_prefix("0x").or_else(|| xor.strip_prefix("0X")) {
            u8::from_str_radix(h, 16)?
        } else {
            xor.parse::<u8>()?
        };
        let mut aes = [0u8; 16];
        aes.copy_from_slice(b);
        Ok(Self { aes, xor: xor_v })
    }
}
