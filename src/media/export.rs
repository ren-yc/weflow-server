//! Media export pipeline: locate the WeChat-side source file for a message's
//! media, decode it, and copy it into the API export directory.
//!
//! Sources (verified against a real 4.1.12 account):
//! - images: `msg/attach/<md5(sessionId)>/<yyyy-MM>/Img/<md5>.dat`
//!   (`hardlink.db:image_hardlink_info_v4` maps md5 → the same file name)
//! - voices: `media_*.db:VoiceInfo(svr_id, voice_data)` — silk bytes prefixed
//!   by one status byte (`0x02 #!SILK_V3…`)
//! - videos: `msg/video/<yyyy-MM>/<md5>.mp4` (plaintext mp4 in practice)
//! - emojis: `emoticon.db` cdn url (external link, no local file needed)
//!
//! DB-backed lookups take explicit `&Connection`s so the caller decides
//! whether they come from live pooled connections or test fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::keystore::ImageKeys;
use crate::media::{self, DatFormat};

/// Filesystem-only context (no database handles).
pub struct ExportCtx {
    /// The live account directory (`…/<wxid>`) that holds `msg/`.
    pub account_dir: PathBuf,
    /// Export destination root (`api-media/`).
    pub export_dir: PathBuf,
    pub media_keys: Option<ImageKeys>,
}

#[derive(Debug, Clone)]
pub struct ExportedMedia {
    /// File name inside the export dir (`<md5>.jpg`, `<svr>.silk`, …).
    pub file_name: String,
    /// Sub directory kind: images | voices | videos | emojis
    pub kind_dir: &'static str,
    /// Written local path (empty for external urls).
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
        .args(["-y", "-loglevel", "error", "-i"])
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
    export_dir: &Path,
    talker: &str,
    kind_dir: &'static str,
    file_name: &str,
    bytes: &[u8],
) -> Option<ExportedMedia> {
    // Both of these become path components. `talker` reaches here from the
    // store (external data — the WeChat database), and `file_name` is derived
    // from the message's own `md5` XML attribute, which the *sender* controls
    // and `parser::attr` does not validate. Today a traversal payload is stopped
    // further up by accident rather than by design — the image path's md5
    // integrity gate cannot match a non-digest string, and `walk_find` compares
    // against `file_name()`, which never holds a separator — so nothing here
    // depends on those staying true. Enforce containment at the join instead.
    if !crate::pathsafe::safe_segment(talker) || !crate::pathsafe::safe_segment(file_name) {
        tracing::warn!(
            "[media-export] 拒绝异常路径分量: talker={talker:?} file_name={file_name:?}"
        );
        return None;
    }
    let dir = export_dir.join(talker).join(kind_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(file_name);
    // idempotent: identical content already exported
    if let Ok(existing) = std::fs::read(&path)
        && existing == bytes
    {
        return Some(ExportedMedia {
            file_name: file_name.to_string(),
            kind_dir,
            local_path: path,
            external_url: None,
        });
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

/// Resolve + export one media item against an already-open set of auxiliary
/// connections (`hardlink/hardlink.db`, `message/media_*.db`,
/// `emoticon/emoticon.db` — supplied by the sync layer from its live pool or
/// from test fixtures). Returns `None` when the source cannot be located or
/// decoded.
pub fn export_one(
    ctx: &ExportCtx,
    aux: &HashMap<String, Connection>,
    talker: &str,
    kind: crate::parser::MediaKind,
    md5: Option<&str>,
    server_id: i64,
) -> Option<ExportedMedia> {
    use crate::parser::MediaKind as K;
    match kind {
        K::Image => {
            let img_md5 = md5?;
            let resolved = aux
                .get("hardlink/hardlink.db")
                .and_then(|conn| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT file_name FROM image_hardlink_info_v4 WHERE md5 = ?1 LIMIT 1",
                        )
                        .ok()?;
                    stmt.query_row([img_md5], |r| r.get::<_, String>(0)).ok()
                })
                .unwrap_or_else(|| format!("{img_md5}.dat"));
            let session_md5 = format!("{:x}", {
                use md5::Digest;
                let mut h = md5::Md5::new();
                h.update(talker.as_bytes());
                h.finalize()
            });
            let attach = ctx.account_dir.join("msg").join("attach");
            let mut candidates = Vec::new();
            let scoped = attach.join(&session_md5);
            if let Ok(months) = std::fs::read_dir(&scoped) {
                for m in months.flatten() {
                    let p = m.path().join("Img").join(&resolved);
                    if p.is_file() {
                        candidates.push(p);
                    }
                }
            }
            if candidates.is_empty() {
                walk_find(&scoped, &resolved, 4, &mut candidates);
            }
            if candidates.is_empty() {
                walk_find(&attach, &resolved, 4, &mut candidates);
            }
            let src = candidates.first()?.clone();
            let raw = std::fs::read(&src).ok()?;
            let decoded = match media::detect_format(&raw) {
                Some(fmt @ (DatFormat::V1 | DatFormat::V2)) => {
                    // V1 uses the fixed built-in key; V2 requires registered keys
                    let keys = if fmt == DatFormat::V1 {
                        Some(ImageKeys { aes: *media::V1_FIXED_AES_KEY, xor: 0 })
                    } else {
                        ctx.media_keys
                    };
                    let keys = keys?;
                    media::decrypt_dat_payload(&raw, &keys.aes, keys.xor)?
                }
                Some(DatFormat::LegacyXor) => {
                    let keys = ctx.media_keys?;
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
                return None; // still compressed — wrong key, refuse garbage
            }
            // Integrity gate: `img_md5` is WeChat's md5 of the *original* image,
            // so the decoded plaintext must hash to it. A mismatch means the key
            // or the segmentation is wrong; without this check such files are
            // written out and only some of them fail to decode downstream, so
            // the rest become silent garbage. Checked before any transcode,
            // which by definition changes the bytes.
            let actual_md5 = format!("{:x}", {
                use md5::Digest;
                let mut h = md5::Md5::new();
                h.update(&decoded);
                h.finalize()
            });
            if !actual_md5.eq_ignore_ascii_case(img_md5) {
                tracing::warn!(
                    "image {img_md5} decoded to md5 {actual_md5} ({} bytes from {}): \
                     refusing to export corrupt bytes",
                    decoded.len(),
                    src.display(),
                );
                return None;
            }
            let (bytes, ext) = if decoded.starts_with(b"wxgf") {
                match wxgf_to_png(&decoded) {
                    Some(png) => (png, "png"),
                    None => {
                        // raw HEVC still: no common image decoder handles it, so
                        // say so rather than shipping an undecodable `.wxgf`
                        // silently. `wxgf_to_png` also returns None when ffmpeg
                        // exists but the decode fails, so don't claim a cause.
                        tracing::warn!(
                            "image {img_md5}: wxgf → png conversion failed \
                             (ffmpeg missing or decode error); exporting raw \
                             wxgf, which most clients cannot decode"
                        );
                        (decoded, "wxgf")
                    }
                }
            } else {
                let e = sniff_image_ext(&decoded);
                (decoded, e)
            };
            write_out(&ctx.export_dir, talker, "images", &format!("{img_md5}.{ext}"), &bytes)
        }
        K::Voice => {
            let svr_id = server_id;
            for conn in aux.values().filter(|_| true) {
                let Ok(mut stmt) =
                    conn.prepare("SELECT voice_data FROM VoiceInfo WHERE svr_id = ?1 ORDER BY data_index ASC")
                else {
                    continue;
                };
                let frags: Vec<Option<Vec<u8>>> = match stmt.query_map([svr_id], |r| {
                    r.get::<_, Option<Vec<u8>>>(0)
                }) {
                    Ok(it) => it.flatten().collect(),
                    Err(_) => continue,
                };
                if frags.is_empty() {
                    continue;
                }
                let mut data = Vec::new();
                for frag in frags.into_iter().flatten() {
                    let start = frag.windows(6).position(|w| w == b"#!SILK").unwrap_or(0);
                    data.extend_from_slice(&frag[start..]);
                }
                if data.is_empty() {
                    continue;
                }
                return write_out(
                    &ctx.export_dir,
                    talker,
                    "voices",
                    &format!("voice_{svr_id}.silk"),
                    &data,
                );
            }
            None
        }
        K::Video => {
            let video_md5 = md5?;
            let fname = aux
                .get("hardlink/hardlink.db")
                .and_then(|conn| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT file_name FROM video_hardlink_info_v4 WHERE md5 = ?1 LIMIT 1",
                        )
                        .ok()?;
                    stmt.query_row([video_md5], |r| r.get::<_, String>(0)).ok()
                })
                .unwrap_or_else(|| format!("{video_md5}.mp4"));
            let video_root = ctx.account_dir.join("msg").join("video");
            let mut hits = Vec::new();
            walk_find(&video_root, &fname, 3, &mut hits);
            let src = hits.first()?.clone();
            let bytes = std::fs::read(&src).ok()?;
            if bytes.starts_with(&media::MAGIC_V1) || bytes.starts_with(&media::MAGIC_V2) {
                // encrypted video stream (ISAAC-64) — not yet supported
                return None;
            }
            write_out(&ctx.export_dir, talker, "videos", &fname, &bytes)
        }
        K::Emoji => {
            let emoji_md5 = md5?;
            let conn = aux.get("emoticon/emoticon.db")?;
            for table in ["kNonStoreEmoticonTable", "EmoticonInfo"] {
                let Ok(mut stmt) = conn.prepare(&format!(
                    "SELECT cdn_url FROM \"{table}\" WHERE lower(hex(md5)) = lower(?1) LIMIT 1"
                )) else {
                    continue;
                };
                if let Ok(url) = stmt.query_row([emoji_md5], |r| r.get::<_, String>(0))
                    && !url.is_empty()
                {
                    return Some(ExportedMedia {
                        file_name: format!("{emoji_md5}.gif"),
                        kind_dir: "emojis",
                        local_path: PathBuf::new(),
                        external_url: Some(url),
                    });
                }
            }
            None
        }
        K::File => None, // files: v1.6 (plaintext tree, low value)
    }
}

/// Run a batch; returns localId → outcome for messages that produced media.
///
/// `aux` maps auxiliary db rel-paths to open read-only connections (the sync
/// layer supplies live pooled connections; tests may supply fixtures).
pub fn export_batch(
    ctx: &ExportCtx,
    aux: &HashMap<String, Connection>,
    jobs: &[(i64, crate::parser::MediaKind, Option<String>, i64, String)],
    max_items: usize,
) -> HashMap<i64, ExportedMedia> {
    let mut out = HashMap::new();
    for (local_id, kind, md5, server_id, talker) in jobs.iter().take(max_items) {
        if let Some(m) =
            export_one(ctx, aux, talker, *kind, md5.as_deref(), *server_id)
        {
            out.insert(*local_id, m);
        }
    }
    out
}

/// Live-mode batch export: the sync layer supplies `storage` (db_storage
/// root) and registration keys; auxiliary databases are opened fresh
/// read-only (raw-key path, no KDF) exactly when a job needs them.
// Eight arguments against a threshold of seven. Every one is an independent
// input the caller genuinely has to supply, and they are already grouped into
// `ExportCtx` immediately below; bundling them into a second parameter struct
// would move the argument list rather than shorten it.
#[allow(clippy::too_many_arguments)]
pub fn export_batch_live(
    storage: &Path,
    keys: &crate::keystore::KeyMap,
    account_dir: &Path,
    export_dir: &Path,
    media_keys: Option<ImageKeys>,
    _wxid: &str,
    jobs: &[(i64, crate::parser::MediaKind, Option<String>, i64, String)],
    max_items: usize,
) -> HashMap<i64, ExportedMedia> {
    let ctx = ExportCtx {
        account_dir: account_dir.to_path_buf(),
        export_dir: export_dir.to_path_buf(),
        media_keys,
    };
    let files = crate::db::scan::enum_db_files(storage);
    let mut aux: HashMap<String, Connection> = HashMap::new();
    let open_aux = |rel: &str| -> Option<Connection> {
        let f = files.iter().find(|f| f.rel == rel)?;
        let key = keys.key_for(rel)?;
        crate::db::live::open_read_only(&f.abs, &hex::encode(key.0)).ok()
    };
    // pre-open what this batch needs (keyed by actual job kinds)
    let want_image = jobs.iter().take(max_items)
        .any(|(_, k, _, _, _)| matches!(k, crate::parser::MediaKind::Image));
    let want_video = jobs.iter().take(max_items)
        .any(|(_, k, _, _, _)| matches!(k, crate::parser::MediaKind::Video));
    let want_voice = jobs.iter().take(max_items)
        .any(|(_, k, _, _, _)| matches!(k, crate::parser::MediaKind::Voice));
    let want_emoji = jobs.iter().take(max_items)
        .any(|(_, k, _, _, _)| matches!(k, crate::parser::MediaKind::Emoji));
    if (want_image || want_video)
        && let Some(c) = open_aux("hardlink/hardlink.db")
    {
        aux.insert("hardlink/hardlink.db".into(), c);
    }
    if want_voice {
        for rel in ["message/media_0.db", "message/media_1.db"] {
            if !aux.contains_key(rel)
                && let Some(c) = open_aux(rel)
            {
                aux.insert(rel.into(), c);
            }
        }
    }
    if want_emoji
        && let Some(c) = open_aux("emoticon/emoticon.db")
    {
        aux.insert("emoticon/emoticon.db".into(), c);
    }
    let mut out = HashMap::new();
    for (local_id, kind, md5, server_id, talker) in jobs.iter().take(max_items) {
        if let Some(m) =
            export_one(&ctx, &aux, talker, *kind, md5.as_deref(), *server_id)
        {
            out.insert(*local_id, m);
        }
    }
    out
}
