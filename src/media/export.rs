//! Media export pipeline: locate the WeChat-side source file for a message's
//! media, decode it, and copy it into the API export directory.
//!
//! Sources (verified against a real 4.1.12 account):
//! - images: `hardlink.db:image_hardlink_info_v4` (md5 → `<md5>.dat`) with the
//!   physical file at `msg/attach/<md5(sessionId)>/<yyyy-MM>/Img/<file>`
//! - voices: `media_*.db:VoiceInfo(svr_id, voice_data)` — silk bytes prefixed
//!   by one status byte (`0x02 #!SILK_V3…`)
//! - videos: `hardlink.db:video_hardlink_info_v4` (md5 → `<md5>.mp4`) at
//!   `msg/video/<yyyy-MM>/<file>` (plaintext mp4 in current accounts)
//! - emojis: `emoticon.db` cdn url (external link, no local file needed)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::keystore::ImageKeys;
use crate::media::{self, DatFormat};

pub struct ExportCtx {
    /// The live account directory (`…/<wxid>`) that holds `msg/`.
    pub account_dir: PathBuf,
    /// Decrypted snapshot root for this account (mirror `<root>/<wxid>/`),
    /// providing `hardlink/hardlink.db`, `message/media_*.db`, `emoticon/emoticon.db`.
    pub snapshot_root: PathBuf,
    /// Export destination root (`api-media/`).
    pub export_dir: PathBuf,
    pub media_keys: Option<ImageKeys>,
    pub wxid: String,
}

#[derive(Debug, Clone)]
pub struct ExportedMedia {
    /// File name inside the export dir (`<md5>.jpg`, `<svr>.silk`, …).
    pub file_name: String,
    /// Sub directory kind: images | voices | videos | emojis
    pub kind_dir: &'static str,
    /// Written local path.
    pub local_path: PathBuf,
    /// External URL (emoji cdn) — when set, no local file was written.
    pub external_url: Option<String>,
}

fn sniff_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"wxgf") {
        // WeChat Graphics Format = raw HEVC still; converted to PNG when an
        // ffmpeg binary is available (see `wxgf_to_png`), else kept raw.
        "wxgf"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else {
        "bin"
    }
}

/// Locate a usable ffmpeg binary: explicit env override, the WeFlow install's
/// own bundled ffmpeg-static, then PATH.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("WEFLOW_SERVER_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    const WELL_KNOWN: &str =
        r"C:\Program Files\WeFlow\resources\app.asar.unpacked\node_modules\ffmpeg-static\ffmpeg.exe";
    let wk = PathBuf::from(WELL_KNOWN);
    if wk.is_file() {
        return Some(wk);
    }
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("ffmpeg.exe");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Decode a wxgf (raw HEVC still) into PNG bytes via ffmpeg.
pub fn wxgf_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let ff = find_ffmpeg()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let inp = std::env::temp_dir().join(format!("wfs_wxgf_{stamp}.hevc"));
    let outp = std::env::temp_dir().join(format!("wfs_wxgf_{stamp}.png"));
    std::fs::write(&inp, bytes).ok()?;
    let res = std::process::Command::new(&ff)
        .args([
            "-y",
            "-loglevel",
            "error",
            "-i",
        ])
        .arg(&inp)
        .args(["-frames:v", "1", "-f", "image2"])
        .arg(&outp)
        .output();
    let _ = std::fs::remove_file(&inp);
    let out = res.ok()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&outp);
        return None;
    }
    let png = std::fs::read(&outp).ok()?;
    let _ = std::fs::remove_file(&outp);
    if png.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(png)
    } else {
        None
    }
}

fn find_hardlink_file(
    snapshot_root: &Path,
    table: &str,
    md5: &str,
) -> Option<String> {
    let db = snapshot_root.join("hardlink").join("hardlink.db");
    let conn = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT file_name FROM \"{table}\" WHERE md5 = ?1 LIMIT 1"
    )) else {
        return None;
    };
    stmt.query_row([md5], |r| r.get::<_, String>(0)).ok()
}

/// Locate `pattern` under `root`, walking at most `depth` directory levels.
fn walk_find(root: &Path, pattern: &str, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= 4 || depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_find(&p, pattern, depth - 1, out);
            if out.len() >= 4 {
                return;
            }
        } else if p.file_name().map(|f| f == pattern).unwrap_or(false) {
            out.push(p);
            if out.len() >= 4 {
                return;
            }
        }
    }
}

fn write_out(
    ctx: &ExportCtx,
    talker: &str,
    kind_dir: &'static str,
    file_name: &str,
    bytes: &[u8],
) -> Option<ExportedMedia> {
    let dir = ctx.export_dir.join(talker).join(kind_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(file_name);
    // idempotent: identical content already exported
    if let Ok(existing) = std::fs::read(&path) {
        if existing == bytes {
            return Some(ExportedMedia {
                file_name: file_name.to_string(),
                kind_dir,
                local_path: path,
                external_url: None,
            });
        }
    }
    let tmp = dir.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, bytes).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(ExportedMedia {
        file_name: file_name.to_string(),
        kind_dir,
        local_path: path,
        external_url: None,
    })
}

/// Resolve + export one media item. Returns `None` when the source cannot be
/// located or decoded (caller keeps metadata-only media).
pub fn export_media(
    ctx: &ExportCtx,
    talker: &str,
    kind: crate::parser::MediaKind,
    md5: Option<&str>,
    server_id: i64,
) -> Option<ExportedMedia> {
    use crate::parser::MediaKind as K;
    match kind {
        K::Image => export_image(ctx, talker, md5?),
        K::Voice => export_voice(ctx, talker, server_id),
        K::Video => export_video(ctx, talker, md5?),
        K::Emoji => export_emoji(ctx, md5?),
        K::File => None, // files: v1.6 (msg/file tree is plaintext; low value)
    }
}

fn export_image(ctx: &ExportCtx, talker: &str, img_md5: &str) -> Option<ExportedMedia> {
    let fname = format!("{img_md5}.dat");
    let hardlink = find_hardlink_file(&ctx.snapshot_root, "image_hardlink_info_v4", img_md5)
        .unwrap_or_else(|| fname.clone());
    // candidate locations, most specific first
    let session_md5 = format!("{:x}", {
        use md5::Digest;
        let mut h = md5::Md5::new();
        h.update(talker.as_bytes());
        h.finalize()
    });
    let attach = ctx.account_dir.join("msg").join("attach");
    let mut candidates = Vec::new();
    let scoped = attach.join(&session_md5);
    // attach/<session_md5>/<month>/Img/<fname>
    if let Ok(months) = std::fs::read_dir(&scoped) {
        for m in months.flatten() {
            let p = m.path().join("Img").join(&hardlink);
            if p.is_file() {
                candidates.push(p);
            }
        }
    }
    if candidates.is_empty() {
        // bounded recursive fallback under the session dir and then globally
        walk_find(&scoped, &hardlink, 4, &mut candidates);
    }
    if candidates.is_empty() {
        walk_find(&attach, &hardlink, 4, &mut candidates);
    }
    let src = candidates.first()?.clone();
    let raw = std::fs::read(&src).ok()?;
    // decode: V1/V2 dat → image; legacy xor needs code; plain image passes through
    let decoded = match media::detect_format(&raw) {
        Some(fmt @ (DatFormat::V1 | DatFormat::V2)) => {
            // V1 uses the fixed built-in key; V2 requires registered keys
            let keys = if fmt == DatFormat::V1 {
                Some(ImageKeys { aes: *media::V1_FIXED_AES_KEY, xor: 0 })
            } else {
                ctx.media_keys.as_ref().copied()
            };
            let Some(keys) = keys else { return None };
            media::decrypt_dat_payload(&raw, &keys.aes, keys.xor)?
        }
        Some(DatFormat::LegacyXor) => {
            let keys = ctx.media_keys.as_ref()?;
            media::decrypt_dat_legacy(&raw, keys.xor)
        }
        None => {
            // maybe an unencrypted cache hit
            if raw.starts_with(&[0xFF, 0xD8, 0xFF])
                || raw.starts_with(&[0x89, b'P', b'N', b'G'])
                || raw.starts_with(b"GIF8")
            {
                raw
            } else {
                return None;
            }
        }
    };
    if decoded.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return None; // still compressed — wrong key, refuse to export garbage
    }
    // wxgf (raw HEVC still) → PNG when ffmpeg is available
    let (bytes, ext) = if decoded.starts_with(b"wxgf") {
        match wxgf_to_png(&decoded) {
            Some(png) => (png, "png"),
            None => (decoded, "wxgf"),
        }
    } else {
        let e = sniff_image_ext(&decoded);
        (decoded, e)
    };
    write_out(ctx, talker, "images", &format!("{img_md5}.{ext}"), &bytes)
}

fn export_voice(ctx: &ExportCtx, talker: &str, svr_id: i64) -> Option<ExportedMedia> {
    let media_dir = ctx.snapshot_root.join("message");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&media_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .map(|f| f.starts_with("media_") && f.ends_with(".db"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    for db in entries {
        let Ok(conn) =
            Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT voice_data FROM VoiceInfo WHERE svr_id = ?1 ORDER BY data_index ASC")
        else {
            continue;
        };
        let mut rows = match stmt.query_map([svr_id], |r| r.get::<_, Option<Vec<u8>>>(0)) {
            Ok(iter) => iter.flatten().collect::<Vec<_>>(),
            Err(_) => continue,
        };
        if !rows.is_empty() {
            // strip each fragment's leading status byte before the `#!SILK`
            // magic, then join in data_index order ('0' first)
            let mut data = Vec::new();
            for frag in rows.drain(..) {
                let Some(frag) = frag else { continue };
                let start = frag.windows(6).position(|w| w == b"#!SILK").unwrap_or(0);
                data.extend_from_slice(&frag[start..]);
            }
            if data.is_empty() {
                continue;
            }
            return write_out(
                ctx,
                talker,
                "voices",
                &format!("voice_{svr_id}.silk"),
                &data,
            );
        }
    }
    None
}

fn export_video(ctx: &ExportCtx, talker: &str, video_md5: &str) -> Option<ExportedMedia> {
    let fname = find_hardlink_file(&ctx.snapshot_root, "video_hardlink_info_v4", video_md5)
        .unwrap_or_else(|| format!("{video_md5}.mp4"));
    let video_root = ctx.account_dir.join("msg").join("video");
    let mut hits = Vec::new();
    walk_find(&video_root, &fname, 3, &mut hits);
    let src = hits.first()?.clone();
    let bytes = std::fs::read(&src).ok()?;
    // encrypted streams carry the dat V1/V2 headers — not yet supported
    if bytes.starts_with(&media::MAGIC_V1) || bytes.starts_with(&media::MAGIC_V2) {
        return None;
    }
    write_out(ctx, talker, "videos", &fname, &bytes)
}

fn export_emoji(ctx: &ExportCtx, emoji_md5: &str) -> Option<ExportedMedia> {
    let db = ctx.snapshot_root.join("emoticon").join("emoticon.db");
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    for table in ["kNonStoreEmoticonTable", "EmoticonInfo"] {
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT cdn_url FROM \"{table}\" WHERE lower(hex(md5)) = lower(?1) LIMIT 1"
        )) else {
            continue;
        };
        if let Ok(url) = stmt.query_row([emoji_md5], |r| r.get::<_, String>(0)) {
            if !url.is_empty() {
                return Some(ExportedMedia {
                    file_name: format!("{emoji_md5}.gif"),
                    kind_dir: "emojis",
                    local_path: PathBuf::new(),
                    external_url: Some(url),
                });
            }
        }
    }
    None
}

/// Export a batch; returns localId → outcome for messages that produced media.
pub fn export_batch(
    ctx: &ExportCtx,
    jobs: &[(i64, crate::parser::MediaKind, Option<String>, i64, String)],
    max_items: usize,
) -> HashMap<i64, ExportedMedia> {
    let mut out = HashMap::new();
    for (local_id, kind, md5, server_id, talker) in jobs.iter().take(max_items) {
        if let Some(m) = export_media(ctx, talker, *kind, md5.as_deref(), *server_id) {
            out.insert(*local_id, m);
        }
    }
    out
}
