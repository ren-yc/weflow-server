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

/// 导出媒体的对外地址：**根相对路径，且不含任何凭据**。
///
/// 两条都是刻意的：① token 是只走请求头的凭据，一旦拼进 URL 就会被复制到响应体、
/// 客户端日志与任何中间缓存里；② 相对路径不把服务基址烤进响应，反代或换端口之后
/// 下发的地址仍然有效。调用方按自己的基址拼接。
pub fn exported_media_url(file_name: &str) -> String {
    format!("/api/v1/media/{file_name}")
}

/// 文件名是否**由内容摘要派生**（形如 32 位十六进制 + 扩展名）。
///
/// 为什么按名取字节的路由只接受这种名字：它要遍历**所有**会话的导出目录，而「同名」在
/// 别的会话里完全可能是另一个文件的内容。摘要派生的名字天然内容唯一 —— 同名即同内容，
/// 遍历的结果因此是确定的。反过来，DB 名回落（视频的 `video_hardlink_info_v4.file_name`）
/// 与原文件名回落都不具备这个性质：它们可以作**元数据**下发，但不能当**句柄**。
pub fn name_is_content_digest(name: &str) -> bool {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit())
}

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
    /// 文件名是否由内容摘要派生（见同名函数）。
    ///
    /// 只有它为真、且**确实写出了本地文件**时，这个名字才可以作为句柄下发（原生面的
    /// `mediaId`、消息面 `media.fileName` 的可用形态）。非摘要派生的名字仍作为
    /// 元数据照给 —— 「这条有媒体、叫什么」与「字节取得到」是两件事。
    pub digest_named: bool,
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

/// `MediaKind` → 导出子目录。与 `write_out` 的三处调用点用的是**同一套字符串** ——
/// 写两遍必然漂移，而漂移的表现是「导出到了 `images/`、查找却去 `photos/`」，两边都静默。
///
/// 返回 `None` 表示这个类型**本就不参与导出**（文件附件就是）：没有目录可找，
/// 因此永远不该通告它的 id。
pub fn kind_dir_for(kind: crate::parser::MediaKind) -> Option<&'static str> {
    use crate::parser::MediaKind;
    match kind {
        MediaKind::Image => Some("images"),
        MediaKind::Voice => Some("voices"),
        MediaKind::Video => Some("videos"),
        MediaKind::Emoji => Some("emojis"),
        _ => None,
    }
}
/// 参与导出的四个类型目录，**唯一一份**。
///
/// 它同时是「按名取字节」那条路由的白名单与 `digest_handles` 的扫描范围。此前这两个用途各自
/// 持有一份同样的数组，而数组写两遍必然漂移（漂移的表现是「导出到了 `images/`、查找却去
/// `photos/`」，两边都静默）。`kind_dir_for` 的每个返回值都必须在这里出现 —— 由
/// `kind_dir_matches_the_export_layout` 那条测试钉住。
pub const EXPORT_TYPE_DIRS: [&str; 4] = ["images", "voices", "videos", "emojis"];
/// 本会话导出目录里**由内容摘要派生**的那些文件：`摘要干 → 实际落盘名`。
///
/// 为什么按「干」而不是按全名查：图片的落盘名扩展名是**解码后嗅探**出来的（可能与消息 XML
/// 里的属性名不同，wxgf 还可能被转成 png），拿元数据里的名字去 stat 会漏报 —— 而漏报的表现是
/// 「拉取面不说 mediaId」，调用方只能多跑一趟导出，永远看不到这里其实已经有字节。
///
/// 只收摘要派生名（同 `fetchable_media_id` 的判据）：非摘要名（语音的 `voice_<svr>.silk`、视频
/// 的平台名）在别的会话里可能是同名异内容的文件，按名跨会话解析不安全，因此不作句柄。
///
/// 同一摘要在多个类型目录里都出现时取**字典序第一个**：内容相同、扩展名只是容器命名差异，
/// 两个名字指向的字节一致，取哪个都对；固定顺序是为了让响应可复现。
pub fn digest_handles(export_dir: &Path, talker: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if !crate::pathsafe::safe_segment(talker) {
        return out;
    }
    let base = export_dir.join(talker);
    for dir in EXPORT_TYPE_DIRS {
        let Ok(entries) = std::fs::read_dir(base.join(dir)) else { continue };
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if !e.path().is_file() || !name_is_content_digest(&name) {
                continue;
            }
            let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&name).to_ascii_lowercase();
            out.entry(stem).or_insert_with(|| name.clone());
        }
    }
    out
}


/// 媒体 id 的「**出现即可取**」判据：导出根下确有这个文件才通告。
///
/// 承诺是**出现即可取**，不是尽力而为 —— 通告一个取不到的 id，只会让调用方拿到 404 并以为
/// 是服务坏了。反过来（能取到却没通告）代价小得多：调用方仍可先 `media=1` 触发导出、再取。
///
/// 只做**一次直接 stat**：布局是 `<root>/<会话>/<类型>/<文件>`，三段都已知，不需要像
/// `find_exported` 那样遍历所有会话（那是 (会话数 × 4) 次 stat，放在每个推送事件上不可接受）。
pub fn fetchable_media_id(
    export_dir: &Path,
    talker: &str,
    kind_dir: &str,
    file_name: &str,
) -> Option<String> {
    // 两个分量都会进路径拼接，规则与导出侧一致（`write_out` 同样先过 `safe_segment`）。
    if !crate::pathsafe::safe_segment(talker) || !crate::pathsafe::safe_segment(file_name) {
        return None;
    }
    // 名字必须**由内容摘要派生**：本函数只 stat 本会话的目录，看不到别处是否躺着同名异内容的
    // 文件，而按名取字节是跨会话解析的。通告一个非摘要派生的名字，可能把别的会话的同名文件
    // 当成它 —— 那正是「出现即可取」被破坏的形态。
    if !name_is_content_digest(file_name) {
        return None;
    }
    let path = export_dir.join(talker).join(kind_dir).join(file_name);
    path.is_file().then(|| file_name.to_string())
}
fn write_out(
    export_dir: &Path,
    talker: &str,
    kind_dir: &'static str,
    file_name: &str,
    bytes: &[u8],
    digest_named: bool,
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
            digest_named,
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
        digest_named,
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
            // 图片的落盘名是 32 位摘要 + 嗅探出的扩展名 ⇒ 摘要派生。
            write_out(&ctx.export_dir, talker, "images", &format!("{img_md5}.{ext}"), &bytes, true)
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
                // 语音的落盘名来自服务端序号（voice_<svr_id>.silk），**不是**内容摘要派生：
                // 别的会话可以有同一个 svr_id 的另一个文件。它照常作为元数据下发，但不作句柄。
                return write_out(
                    &ctx.export_dir,
                    talker,
                    "voices",
                    &format!("voice_{svr_id}.silk"),
                    &data,
                    false,
                );
            }
            None
        }
        K::Video => {
            let video_md5 = md5?;
            // 两个来源的性质不同，必须分开：库里查到的文件名是**平台给的名字**，别的会话
            // 可能有同名但内容不同的文件 ⇒ 只作元数据；回落出来的 "<md5>.mp4" 是摘要派生的
            // ⇒ 可以作句柄。把两者混成一个名字，等于让「出现即可取」在最需要它的那一端失效。
            let from_db = aux
                .get("hardlink/hardlink.db")
                .and_then(|conn| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT file_name FROM video_hardlink_info_v4 WHERE md5 = ?1 LIMIT 1",
                        )
                        .ok()?;
                    stmt.query_row([video_md5], |r| r.get::<_, String>(0)).ok()
                });
            let (fname, digest_named) = match from_db {
                Some(name) => (name, false),
                None => (format!("{video_md5}.mp4"), true),
            };
            let video_root = ctx.account_dir.join("msg").join("video");
            let mut hits = Vec::new();
            walk_find(&video_root, &fname, 3, &mut hits);
            let src = hits.first()?.clone();
            let bytes = std::fs::read(&src).ok()?;
            if bytes.starts_with(&media::MAGIC_V1) || bytes.starts_with(&media::MAGIC_V2) {
                // encrypted video stream (ISAAC-64) — not yet supported
                return None;
            }
            write_out(&ctx.export_dir, talker, "videos", &fname, &bytes, digest_named)
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
                        // 名字确实是摘要派生的，但它只有外链、没有本地文件 ⇒
                        // 句柄判据里的「文件存在」那一半会挡住它。
                        digest_named: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 导出媒体的对外地址必须是**根相对路径、且不含任何凭据**。
    ///
    /// 回归点：它曾经是绝对 URL 且在末尾拼 `?access_token=`——于是 token 被复制进
    /// 响应体、客户端日志与任何中间缓存；绝对形态还会把服务基址烤进响应，反代或换
    /// 端口之后下发的是失效地址。
    ///
    /// 放在单元测试而不是 HTTP 测试里，是因为假夹具造不出可导出的媒体源文件，
    /// 而真实账号的下游测试是 `#[ignore]` 的——两者都不会给这条回归兜底。
    #[test]
    fn exported_media_url_is_relative_and_carries_no_credential() {
        let url = exported_media_url("aabbccddeeff00112233445566778899.jpg");
        assert_eq!(url, "/api/v1/media/aabbccddeeff00112233445566778899.jpg");
        assert!(url.starts_with('/'), "根相对路径以 / 开头");
        assert!(!url.starts_with("http"), "不得把服务基址烤进响应");
        assert!(!url.contains("access_token"), "响应体里不得出现凭据");
    }


    /// 「**出现即可取**」是承诺：导出根下确有文件才通告 id。
    ///
    /// 反面同样重要：能取到却没通告的代价小得多（调用方仍可先 `media=1` 触发导出再取），
    /// 而通告一个取不到的 id 会让调用方拿到 404 并以为是服务坏了 —— 两个方向的代价不对称。
    #[test]
    fn media_id_is_only_advertised_when_the_file_is_there() {
        let root = std::env::temp_dir().join(format!("wfs_media_id_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("wxid_a@chatroom").join("images");
        std::fs::create_dir_all(&dir).unwrap();
        let name = "aabbccddeeff00112233445566778899.jpg";
        std::fs::write(dir.join(name), b"x").unwrap();
        // 非摘要派生的名字（语音的 voice_<svr_id>.silk 就是这一形态）：文件在也不通告。
        std::fs::write(dir.join("voice_123.silk"), b"x").unwrap();

        assert_eq!(
            fetchable_media_id(&root, "wxid_a@chatroom", "images", name).as_deref(),
            Some(name),
            "文件在且名字是摘要派生 ⇒ 通告"
        );
        assert_eq!(
            fetchable_media_id(&root, "wxid_a@chatroom", "images", "00112233445566778899aabbccddeeff.jpg"),
            None,
            "文件不在 ⇒ 不通告（这正是「出现即可取」）"
        );
        assert_eq!(
            fetchable_media_id(&root, "wxid_a@chatroom", "voices", name),
            None,
            "类型目录也要对得上"
        );
        assert_eq!(
            fetchable_media_id(&root, "wxid_a@chatroom", "images", "voice_123.silk"),
            None,
            "名字不是摘要派生 ⇒ 不通告：按名取字节是跨会话解析的，同名可能是别的文件"
        );
        // 路径分量守卫：与导出侧同一套规则。
        assert_eq!(fetchable_media_id(&root, "../..", "images", name), None);
        assert_eq!(fetchable_media_id(&root, "wxid_a@chatroom", "images", "../x"), None);
        // 判据本身：32 位十六进制（大小写都算）+ 扩展名。
        assert!(name_is_content_digest(name));
        assert!(name_is_content_digest("AABBCCDDEEFF00112233445566778899.mp4"));
        assert!(name_is_content_digest("aabbccddeeff00112233445566778899"));
        assert!(!name_is_content_digest("voice_123.silk"));
        assert!(!name_is_content_digest("aabbccddeeff0011223344556677889.jpg"));
        assert!(!name_is_content_digest("1.mp4"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 越界分量在**写入点**被拒：`talker` 来自库（外部数据）、`file_name` 派生自
    /// 消息自带的 md5 属性（**发送方可控**）——两者都过 `safe_segment` 才落盘，
    /// 拒绝发生在 join 之前。这是四个 pathsafe 调用点里唯一「写」的一个：
    /// 它被绕过的代价是任意位置落文件，所以断言既要「拒绝」也要「无残留」，
    /// 还要有正向对照防「全都拒绝」的假绿。
    #[test]
    fn write_out_refuses_traversal_components() {
        let root = std::env::temp_dir().join(format!("wfs_write_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            write_out(&root, "wxid_a", "images", "../pwn.jpg", b"x", true).is_none(),
            "越界 file_name 被拒"
        );
        assert!(
            write_out(&root, "../evil", "images", "ok.jpg", b"x", true).is_none(),
            "越界 talker 被拒（它来自库，不是请求——但正因为不可控才必须拒）"
        );
        assert!(
            write_out(&root, "wxid_a", "images", "a:b.jpg", b"x", true).is_none(),
            "Windows 备用数据流冒号也被拒"
        );
        // 落点必须按 **join 链**算，不能凭直觉：`dir = root/wxid_a/images`，
        // `dir.join("../pwn.jpg")` 归一化到 **root/wxid_a/pwn.jpg**（root 之内，不是 parent）；
        // `root.join("../evil/images")` 归一化到 **root 的父目录下的 evil/**（root 之外）。
        // 原来的两条断言分别查了 root.parent()/pwn.jpg 与 root/evil —— 两个位置都不对，
        // 于是在守卫被移除时它们**仍然是绿的**，只有上面那两条 is_none() 会红。
        assert!(
            !root.join("wxid_a").join("pwn.jpg").exists(),
            "越界 file_name 不得落在 root/wxid_a/ 下"
        );
        assert!(
            !root.parent().unwrap().join("evil").exists(),
            "越界 talker 不得在 root 之外建出目录"
        );

        // 正向对照：合法分量照常写入。
        assert!(write_out(&root, "wxid_a", "images", "ok.jpg", b"x", false).is_some());
        assert!(root.join("wxid_a/images/ok.jpg").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 目录映射与「不参与导出的类型」的判据。
    #[test]
    fn kind_dir_matches_the_export_layout() {
        use crate::parser::MediaKind;
        assert_eq!(kind_dir_for(MediaKind::Image), Some("images"));
        assert_eq!(kind_dir_for(MediaKind::Voice), Some("voices"));
        assert_eq!(kind_dir_for(MediaKind::Video), Some("videos"));
        assert_eq!(kind_dir_for(MediaKind::Emoji), Some("emojis"));
        // 文件附件不参与导出 ⇒ 永远没有目录可找 ⇒ 永远不该通告它的 id。
        assert_eq!(kind_dir_for(MediaKind::File), None);
    }
}