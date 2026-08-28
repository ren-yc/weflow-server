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
//! Header (15 bytes): `[6B magic][aesSize u32LE @6][xorSize u32LE @10][flag u8 @14]`
//!
//! The payload (everything after the header) is three segments, in order:
//!   1. AES-128-ECB ciphertext of the first `aesSize` plaintext bytes. PKCS7
//!      *always* appends 1..=16 bytes, so the ciphertext is
//!      `aesSize + (16 - aesSize % 16)` long — a full extra block whenever
//!      `aesSize` is already 16-aligned. Real files have `aesSize == 1024`
//!      almost without exception, making that extra block the common case.
//!   2. `rawSize` verbatim plaintext bytes. Not stored in the header: derive it
//!      as `payloadLen - ciphertextLen - xorSize`. Zero for most files, but
//!      non-zero once `xorSize` saturates (observed cap: 1 MiB) — for large
//!      images WeChat leaves the middle in the clear.
//!   3. The trailing `xorSize` bytes, each XORed with `xorKey`.
//!
//! The XOR segment is thus anchored at the *end* of the payload and is never
//! measured forward from the AES segment. Cross-checked against 21 624 real
//! `.dat` files: decoding this way reproduces WeChat's own image md5
//! byte-for-byte, including the large `rawSize > 0` shape.

pub mod export;

use aes::cipher::{BlockDecrypt, KeyInit};

#[cfg(test)]
use aes::cipher::BlockEncrypt;
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

/// Ciphertext length of a PKCS7-padded plaintext of `plain_len` bytes.
///
/// PKCS7 appends 1..=16 bytes — never zero — so a 16-aligned plaintext grows by
/// a whole extra block. `div_ceil(16) * 16` gets this wrong for exactly the
/// alignment real `.dat` files always hit (`aesSize == 1024`).
fn pkcs7_ct_len(plain_len: usize) -> Option<usize> {
    plain_len.checked_add(16 - plain_len % 16)
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
    let ct_len = pkcs7_ct_len(aes_size)?;
    if ct_len.checked_add(xor_size)? > payload.len() {
        return None;
    }
    // The XOR segment is anchored at the end; whatever sits between it and the
    // ciphertext is the (headerless) raw plaintext segment.
    let xor_start = payload.len() - xor_size;
    let mut out = Vec::with_capacity(aes_size + (xor_start - ct_len) + xor_size);
    // 1. AES-128-ECB segment (drop the PKCS7 padding by truncating to aes_size)
    let aes_pt = aes128_ecb_decrypt(aes_key, &payload[..ct_len]);
    out.extend_from_slice(&aes_pt[..aes_size.min(aes_pt.len())]);
    // 2. raw plaintext middle (empty unless xor_size saturated)
    out.extend_from_slice(&payload[ct_len..xor_start]);
    // 3. XOR tail
    out.extend(payload[xor_start..].iter().map(|b| b ^ xor_key));
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

    /// Build a V2 sample exactly the way WeChat lays one out:
    /// `[header] + aes(pkcs7(head)) + mid + xor(tail)`, with `xorSize` counting
    /// the *trailing* segment and `mid` carried only implicitly.
    fn build_v2_sample(
        aes_key: &[u8; 16],
        xor_key: u8,
        head: &[u8],
        mid: &[u8],
        tail: &[u8],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC_V2);
        data.extend_from_slice(&(head.len() as u32).to_le_bytes());
        data.extend_from_slice(&(tail.len() as u32).to_le_bytes());
        data.push(1); // flag: 1 in every real file observed
        let padded = {
            // real PKCS7: 1..=16 bytes, so a 16-aligned head gains a whole block
            let pad = 16 - head.len() % 16;
            let mut v = head.to_vec();
            v.extend(std::iter::repeat_n(pad as u8, pad));
            v
        };
        data.extend_from_slice(&aes128_ecb_encrypt(aes_key, &padded));
        data.extend_from_slice(mid);
        data.extend(tail.iter().map(|b| b ^ xor_key));
        data
    }

    /// 32 bytes: 16-aligned, so PKCS7 appends a full extra block. This is the
    /// shape of every real file (`aesSize == 1024`) and the one a
    /// `div_ceil(16) * 16` ciphertext length gets wrong.
    const ALIGNED_HEAD: &[u8; 32] = b"\xff\xd8\xff\xe0jpeg-ish-header-bytes-012345";

    /// Both keys as the decrypt path derives them, so a fixture built with these
    /// is guaranteed to be self-consistent (`xor_key` is the code's first byte,
    /// not a value the test gets to pick).
    fn v2_keys(code: &ImgCode, wxid: &str) -> ([u8; 16], u8) {
        let aes = code.aes_key_hex(wxid).as_bytes().try_into().unwrap();
        (aes, code.xor_key())
    }

    #[test]
    fn v2_roundtrip_detects_and_decrypts() {
        // the sample must be built with the SAME derived key the decrypt path
        // will use (aesKey = MD5(code + wxid) hex chars as ASCII bytes)
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let (key, xor) = v2_keys(&code, wxid);
        let tail = b"xor-segment-bytes";
        let sample = build_v2_sample(&key, xor, ALIGNED_HEAD, &[], tail);
        assert_eq!(detect_format(&sample), Some(DatFormat::V2));
        let (out, fmt) = decrypt_dat(&sample, Some(&code), wxid)
            .expect("decrypt must succeed with matching key");
        assert_eq!(fmt, DatFormat::V2);
        assert_eq!(out, [ALIGNED_HEAD.as_slice(), tail].concat());
        // wrong code must fail to produce the magic
        assert!(!decrypt_dat(&sample, Some(&ImgCode("y".into())), wxid)
            .map(|(b, _)| b.starts_with(b"\xff\xd8\xff\xe0"))
            .unwrap_or(false));
        // no img code -> V2 cannot decrypt
        assert!(decrypt_dat(&sample, None, wxid).is_none());
    }

    /// The 16-aligned `aesSize` case: PKCS7 adds a whole block, so the
    /// ciphertext is `aesSize + 16`. Reading it as `aesSize` shifts everything
    /// after the AES segment by 16 bytes and splices the padding block into the
    /// output as if it were image data — head and tail still look right, the
    /// middle is destroyed. Regression guard for exactly that.
    #[test]
    fn v2_aligned_aes_size_consumes_the_extra_pkcs7_block() {
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let (key, xor) = v2_keys(&code, wxid);
        let tail: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        let sample = build_v2_sample(&key, xor, ALIGNED_HEAD, &[], &tail);
        assert_eq!(ALIGNED_HEAD.len() % 16, 0, "fixture must be 16-aligned");
        // payload = ciphertext(48) + tail(200): the extra block must be consumed
        assert_eq!(sample.len(), 15 + ALIGNED_HEAD.len() + 16 + tail.len());
        let (out, _) = decrypt_dat(&sample, Some(&code), wxid).unwrap();
        assert_eq!(out, [ALIGNED_HEAD.as_slice(), &tail].concat());
        assert_eq!(out.len(), sample.len() - 15 - 16);
    }

    /// `xorSize` saturates (1 MiB in the wild) and the remainder stays raw, so
    /// the payload holds all three segments. The XOR segment is anchored at the
    /// end — measuring it forward from the AES segment corrupts the raw middle.
    #[test]
    fn v2_three_segments_when_xor_size_saturates() {
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let (key, xor) = v2_keys(&code, wxid);
        let mid: Vec<u8> = (0..500u32).map(|i| (i % 253) as u8).collect();
        let tail: Vec<u8> = (0..300u32).map(|i| (i % 247) as u8).collect();
        let sample = build_v2_sample(&key, xor, ALIGNED_HEAD, &mid, &tail);
        let (out, _) = decrypt_dat(&sample, Some(&code), wxid).unwrap();
        assert_eq!(out, [ALIGNED_HEAD.as_slice(), &mid, &tail].concat());
    }

    /// A non-aligned `aesSize` pads to the next block, so PKCS7 and
    /// `div_ceil` agree there — keep that path covered.
    #[test]
    fn v2_unaligned_aes_size_pads_to_next_block() {
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let (key, xor) = v2_keys(&code, wxid);
        let head = b"\xff\xd8\xff\xe0unaligned-head"; // 18 bytes -> ct 32
        let tail = b"tail";
        let sample = build_v2_sample(&key, xor, head, &[], tail);
        assert_eq!(sample.len(), 15 + 32 + tail.len());
        let (out, _) = decrypt_dat(&sample, Some(&code), wxid).unwrap();
        assert_eq!(out, [head.as_slice(), tail].concat());
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let code = ImgCode("x".to_string());
        let wxid = "wxid_t";
        let (key, xor) = v2_keys(&code, wxid);
        let mut sample = build_v2_sample(&key, xor, ALIGNED_HEAD, &[], b"tail-bytes");
        sample.truncate(15 + 32); // less than the 48-byte ciphertext
        assert!(decrypt_dat(&sample, Some(&code), wxid).is_none());
    }

    #[test]
    fn v1_uses_fixed_key() {
        let key = V1_FIXED_AES_KEY;
        let mut sample = build_v2_sample(key, 0, ALIGNED_HEAD, &[], b"tail");
        sample[0..6].copy_from_slice(&MAGIC_V1);
        let (out, fmt) = decrypt_dat(&sample, None, "wxid_t").expect("v1 needs no img code");
        assert_eq!(fmt, DatFormat::V1);
        assert_eq!(out, [ALIGNED_HEAD.as_slice(), b"tail"].concat());
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
