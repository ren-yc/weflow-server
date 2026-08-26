//! WeChat media decryption.
//!
//! v1 scope: `.dat` image cache files (three generations):
//! - legacy: single-byte XOR (no header)
//! - V1 (transition): header `07 08 56 31 08 07`, payload = AES-128-ECB part +
//!   raw part + XOR part; AES key is the fixed string `cfcd208495d565ef`
//! - V2 (2025-08+): same layout, header `07 08 56 32 08 07`, AES key derived
//!   from the registered `img_code`: aesKey = MD5(code+wxid) hex string chars
//!   used as 16 ASCII bytes; xorKey = code & 0xff
//!
//! Layout: `[6B magic][aesSize int32LE @6][xorSize int32LE @10][1B pad @14]`
//! followed by: AES-128-ECB encrypted segment (length = aesSize, padded to a
//! multiple of 16 with PKCS7), then `rawSize` plaintext bytes, then the XOR
//! segment (xorKey applied bytewise). Segment lengths are all relative to the
//! *original* payload within the first AES block (WeFlow/wechat-decrypt).

pub mod export;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

pub const MAGIC_V1: [u8; 6] = [0x07, 0x08, 0x56, 0x31, 0x08, 0x07];
pub const MAGIC_V2: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];
/// Fixed AES key for V1 images (wechat-decrypt ground truth).
pub const V1_FIXED_AES_KEY: &[u8; 16] = b"cfcd208495d565ef";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatFormat {
    V1,
    V2,
    LegacyXor,
}

/// Detect the .dat format from the first bytes.
pub fn detect_format(data: &[u8]) -> Option<DatFormat> {
    if data.len() >= 6 && data[..6] == MAGIC_V1 {
        Some(DatFormat::V1)
    } else if data.len() >= 6 && data[..6] == MAGIC_V2 {
        Some(DatFormat::V2)
    } else if data.len() > 1 {
        // headerless legacy files can still be guessed by content validation
        Some(DatFormat::LegacyXor)
    } else {
        None
    }
}

fn aes128_ecb_decrypt(key: &[u8; 16], ct: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut out = ct.to_vec();
    for block in out.chunks_mut(16) {
        if block.len() == 16 {
            let mut b = [0u8; 16];
            b.copy_from_slice(block);
            cipher.decrypt_block((&mut b).into());
            block.copy_from_slice(&b);
        }
    }
    out
}

/// Test-only encryption helper (used to build synthetic V1/V2 samples).
#[cfg(test)]
fn aes128_ecb_encrypt(key: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut out = pt.to_vec();
    for block in out.chunks_mut(16) {
        if block.len() == 16 {
            let mut b = [0u8; 16];
            b.copy_from_slice(block);
            cipher.encrypt_block((&mut b).into());
            block.copy_from_slice(&b);
        }
    }
    out
}

/// Decrypt a V1/V2 `.dat` payload (after the 15-byte header) into image bytes.
/// `aes_key` is the 16-ASCII-byte key; `xor_key` the XOR byte.
pub fn decrypt_dat_payload(data: &[u8], aes_key: &[u8; 16], xor_key: u8) -> Option<Vec<u8>> {
    if data.len() < 15 {
        return None;
    }
    let aes_size = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
    let xor_size = u32::from_le_bytes(data[10..14].try_into().ok()?) as usize;
    let payload = &data[15..];
    let aes_len = aes_size.div_ceil(16) * 16; // PKCS7-padded AES part
    if aes_len > payload.len() {
        return None;
    }
    let mut out = Vec::with_capacity(aes_size + xor_size + payload.len() - aes_len);
    // AES-128-ECB segment (strip PKCS7 padding)
    let aes_ct = &payload[..aes_len];
    let aes_pt = aes128_ecb_decrypt(aes_key, aes_ct);
    let aes_pt = &aes_pt[..aes_size.min(aes_pt.len())];
    out.extend_from_slice(aes_pt);
    // raw segment (xor_size counts the raw bytes between AES and XOR segments)
    let raw_start = aes_len;
    let raw_end = (raw_start + xor_size).min(payload.len());
    out.extend_from_slice(&payload[raw_start..raw_end]);
    // XOR segment
    for b in &payload[raw_end..] {
        out.push(b ^ xor_key);
    }
    Some(out)
}

/// Decrypt a legacy single-byte XOR file.
pub fn decrypt_dat_legacy(data: &[u8], xor_key: u8) -> Vec<u8> {
    data.iter().map(|b| b ^ xor_key).collect()
}

/// Best-effort decrypt a whole `.dat` file.
/// Returns `(image_bytes, format)`.
pub fn decrypt_dat(data: &[u8], img_code: Option<&crate::keystore::ImgCode>, wxid: &str) -> Option<(Vec<u8>, DatFormat)> {
    match detect_format(data)? {
        DatFormat::V2 => {
            let code = img_code?;
            let key: [u8; 16] = code
                .aes_key_hex(wxid)
                .as_bytes()
                .try_into()
                .ok()?;
            decrypt_dat_payload(data, &key, code.xor_key()).map(|b| (b, DatFormat::V2))
        }
        DatFormat::V1 => decrypt_dat_payload(data, V1_FIXED_AES_KEY, 0).map(|b| (b, DatFormat::V1)),
        DatFormat::LegacyXor => {
            let code = img_code?;
            Some((decrypt_dat_legacy(data, code.xor_key()), DatFormat::LegacyXor))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::ImgCode;

    fn build_v2_sample(aes_key: &[u8; 16], xor_key: u8, raw: &[u8]) -> Vec<u8> {
        // layout: [magic][aesSize@6][xorSize@10][pad@14] + aes(pt) + raw + xor
        let aes_pt = b"\xff\xd8\xff\xe0jpeg-ish-image-bytes-0123456789";
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_V2);
        data.extend_from_slice(&(aes_pt.len() as u32).to_le_bytes());
        data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        data.push(0);
        let padded: Vec<u8> = {
            let rem = aes_pt.len() % 16;
            let mut v = aes_pt.to_vec();
            if rem != 0 {
                v.extend(std::iter::repeat_n(0u8, 16 - rem));
            }
            v
        };
        data.extend_from_slice(&aes128_ecb_encrypt(aes_key, &padded));
        data.extend_from_slice(raw);
        data.extend(raw.iter().map(|b| b ^ xor_key).collect::<Vec<_>>());
        data
    }

    #[test]
    fn v2_roundtrip_detects_and_decrypts() {
        // the sample must be built with the SAME derived key the decrypt path
        // will use (aesKey = MD5(code + wxid) hex chars as ASCII bytes)
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let key_bytes = code.aes_key_hex(wxid);
        let key: [u8; 16] = key_bytes.as_bytes().try_into().unwrap();
        let xor = 0x5A;
        let raw = b"raw-segment-bytes";
        let sample = build_v2_sample(&key, xor, raw);
        assert_eq!(detect_format(&sample), Some(DatFormat::V2));
        let (out, fmt) = decrypt_dat(&sample, Some(&code), wxid)
            .expect("decrypt must succeed with matching key");
        assert_eq!(fmt, DatFormat::V2);
        assert!(out.starts_with(b"\xff\xd8\xff\xe0"));
        assert!(out.windows(raw.len()).any(|w| w == raw), "raw segment present");
        // wrong code must fail to produce the magic
        assert!(!decrypt_dat(&sample, Some(&ImgCode("y".into())), wxid)
            .map(|(b, _)| b.starts_with(b"\xff\xd8\xff\xe0"))
            .unwrap_or(false));
        // no img code -> V2 cannot decrypt
        assert!(decrypt_dat(&sample, None, wxid).is_none());
    }

    #[test]
    fn v1_uses_fixed_key() {
        let key = V1_FIXED_AES_KEY;
        let mut sample = build_v2_sample(key, 0, b"raw");
        sample[0..6].copy_from_slice(&MAGIC_V1);
        let (out, fmt) = decrypt_dat(&sample, None, "wxid_t").expect("v1 needs no img code");
        assert_eq!(fmt, DatFormat::V1);
        assert!(out.starts_with(b"\xff\xd8\xff\xe0"));
    }

    #[test]
    fn legacy_xor_roundtrip() {
        let code = ImgCode("A".into());
        let x = code.xor_key();
        let plain = b"legacy image bytes";
        let enc: Vec<u8> = plain.iter().map(|b| b ^ x).collect();
        let (out, fmt) = decrypt_dat(&enc, Some(&code), "wxid_t").unwrap();
        assert_eq!(fmt, DatFormat::LegacyXor);
        assert_eq!(out, plain);
    }
}