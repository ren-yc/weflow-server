//! Strict local-media export for a uniquely located source message.

use std::{collections::HashMap, path::{Path, PathBuf}};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use crate::{keystore::{ImageKeys, KeyMap}, parser::MediaKind};
use super::{DatFormat, export::{ExportCtx, PreparedExport, ExportedMedia, sniff_image_ext, prepare_out}};

#[derive(Debug)]
pub enum ExportError { Missing, Unsupported, Ambiguous, Failed }

impl From<std::io::Error> for ExportError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound { Self::Missing } else { Self::Failed }
    }
}
impl From<rusqlite::Error> for ExportError {
    fn from(_: rusqlite::Error) -> Self { Self::Failed }
}

pub struct Target<'a> {
    pub talker: &'a str,
    pub kind: MediaKind,
    pub md5: Option<&'a str>,
    pub server_id: i64,
}

/// Open only auxiliary databases relevant to this target. Missing keys are an
/// unsupported export, whereas actual reads/decodes fail observably.
pub fn export_live(storage: &Path, keys: &KeyMap, ctx: &ExportCtx, target: Target<'_>) -> Result<PreparedExport, ExportError> {
    if matches!(target.kind, MediaKind::File | MediaKind::Emoji) { return Err(ExportError::Unsupported); }
    if let Some(existing) = reusable_export(ctx, &target)? { return Ok(existing); }
    let mut files = Vec::new();
    match target.kind {
        MediaKind::Image | MediaKind::Video => {
            let path = storage.join("hardlink/hardlink.db");
            match std::fs::metadata(&path) {
                Ok(_) => files.push(("hardlink/hardlink.db".to_string(), path)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => return Err(e.into()),
            }
        }
        MediaKind::Voice => {
            let entries = std::fs::read_dir(storage.join("message"))?;
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("media_") && name.ends_with(".db") && entry.file_type()?.is_file() {
                    files.push((format!("message/{name}"), entry.path()));
                }
            }
        }
        _ => return Err(ExportError::Unsupported),
    }
    let mut aux = HashMap::new();
    for (relative, path) in files {
        if !crate::pathsafe::is_contained(storage, &path) { return Err(ExportError::Failed); }
        let key = keys.key_for(&relative).ok_or(ExportError::Unsupported)?;
        let conn = crate::db::live::open_read_only(&path, &hex::encode(key.0)).map_err(|_| ExportError::Failed)?;
        aux.insert(relative, conn);
    }
    export_with(ctx, &aux, target)
}

fn reusable_export(ctx: &ExportCtx, target: &Target<'_>) -> Result<Option<PreparedExport>, ExportError> {
    let Some(digest) = target.md5 else { return Ok(None) };
    if digest.len() != 32 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || !crate::pathsafe::safe_segment(target.talker) { return Ok(None); }
    let (kind_dir, extensions): (_, &[&str]) = match target.kind {
        MediaKind::Image => ("images", &["png", "jpg", "gif", "webp", "bmp"]),
        MediaKind::Video => ("videos", &["mp4"]),
        _ => return Ok(None),
    };
    for ext in extensions {
        let file_name = format!("{digest}.{ext}");
        let path = ctx.export_dir.join(target.talker).join(kind_dir).join(&file_name);
        if !path.is_file() { continue; }
        if !crate::pathsafe::is_contained(&ctx.export_dir, &path) { return Err(ExportError::Failed); }
        let bytes = std::fs::read(&path)?;
        if !format!("{:x}", md5::Md5::digest(&bytes)).eq_ignore_ascii_case(digest) { continue; }
        let actual = if kind_dir == "images" { sniff_image_ext(&bytes) }
            else if bytes.get(4..8) == Some(b"ftyp") { "mp4" } else { "bin" };
        if actual != *ext { continue; }
        return Ok(Some(PreparedExport::existing(&ctx.export_dir, ExportedMedia {
            file_name, kind_dir, local_path: path, external_url: None, digest_named: true,
        })));
    }
    Ok(None)
}

fn lookup_name(conn: Option<&Connection>, table: &str, digest: &str) -> Result<Option<String>, ExportError> {
    let Some(conn) = conn else { return Ok(None) };
    let columns = crate::db::open::table_columns_checked(conn, table)?;
    if !["file_name", "md5"].iter().all(|c| columns.iter().any(|s| s.eq_ignore_ascii_case(c))) {
        return Ok(None);
    }
    let mut stmt = conn.prepare(&format!("SELECT DISTINCT file_name FROM {table} WHERE md5=?1 LIMIT 2"))?;
    let names = stmt.query_map([digest], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    if names.len() > 1 { return Err(ExportError::Ambiguous); }
    Ok(names.into_iter().next())
}

fn locate(root: &Path, name: &str, depth: usize) -> Result<Vec<PathBuf>, ExportError> {
    if depth == 0 { return Ok(Vec::new()); }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let canonical = root.canonicalize()?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            // 悬空链接不是候选：跳过它，别让一个无关的坏链接把整个查找判死。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if !resolved.starts_with(&canonical) { continue; }
        if entry.file_type()?.is_dir() {
            found.extend(locate(&path, name, depth - 1)?);
        } else if entry.file_name() == name && entry.file_type()?.is_file() { found.push(resolved); }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn read_unique(root: &Path, paths: Vec<PathBuf>) -> Result<Vec<u8>, ExportError> {
    let mut found = None;
    for path in paths {
        if !crate::pathsafe::is_contained(root, &path) { return Err(ExportError::Failed); }
        let bytes = std::fs::read(path)?;
        if found.as_ref().is_some_and(|previous| *previous != bytes) { return Err(ExportError::Ambiguous); }
        found = Some(bytes);
    }
    found.ok_or(ExportError::Missing)
}

fn image_bytes(raw: Vec<u8>, keys: Option<ImageKeys>, expected: &str) -> Result<Vec<u8>, ExportError> {
    let decoded = if sniff_image_ext(&raw) != "bin" { raw } else { match super::detect_format(&raw) {
        Some(format @ (DatFormat::V1 | DatFormat::V2)) => {
            let keys = if format == DatFormat::V1 { ImageKeys { aes: *super::V1_FIXED_AES_KEY, xor: 0 } }
                else { keys.ok_or(ExportError::Unsupported)? };
            super::decrypt_dat_payload(&raw, &keys.aes, keys.xor).ok_or(ExportError::Failed)?
        }
        Some(DatFormat::LegacyXor) => super::decrypt_dat_legacy(&raw, keys.ok_or(ExportError::Unsupported)?.xor),
        None => raw,
    }};
    let actual = format!("{:x}", md5::Md5::digest(&decoded));
    if !actual.eq_ignore_ascii_case(expected) { return Err(ExportError::Failed); }
    if decoded.starts_with(b"wxgf") {
        return super::export::wxgf_to_png(&decoded).ok_or(ExportError::Failed);
    }
    if sniff_image_ext(&decoded) == "bin" { return Err(ExportError::Failed); }
    Ok(decoded)
}

/// Shared byte validation and guarded writer; callers supply source identity.
pub fn export_with(ctx: &ExportCtx, aux: &HashMap<String, Connection>, target: Target<'_>) -> Result<PreparedExport, ExportError> {
    let (bytes, dir, ext) = match target.kind {
        MediaKind::Image => {
            let digest = target.md5.ok_or(ExportError::Unsupported)?;
            let name = lookup_name(aux.get("hardlink/hardlink.db"), "image_hardlink_info_v4", digest)?
                .unwrap_or_else(|| format!("{digest}.dat"));
            let root = ctx.account_dir.join("msg/attach");
            let scoped = root.join(format!("{:x}", md5::Md5::digest(target.talker.as_bytes())));
            let mut sources = locate(&scoped, &name, 4)?;
            if sources.is_empty() { sources = locate(&root, &name, 4)?; }
            let raw = read_unique(&ctx.account_dir, sources)?;
            let bytes = image_bytes(raw, ctx.media_keys, digest)?;
            let ext = sniff_image_ext(&bytes);
            (bytes, "images", ext)
        }
        MediaKind::Voice => {
            let mut found: Option<Vec<u8>> = None;
            for conn in aux.values() {
                let columns = crate::db::open::table_columns_checked(conn, "VoiceInfo")?;
                if !["voice_data", "svr_id", "data_index"].iter().all(|c| columns.iter().any(|s| s.eq_ignore_ascii_case(c))) { continue; }
                let mut stmt = conn.prepare("SELECT voice_data, data_index FROM VoiceInfo WHERE svr_id=?1 ORDER BY data_index")?;
                let mut rows = stmt.query([target.server_id])?;
                let mut bytes = Vec::new();
                let mut previous = None;
                while let Some(row) = rows.next()? {
                    let index = row.get::<_, rusqlite::types::Value>(1)?;
                    if !matches!(index, rusqlite::types::Value::Integer(_) | rusqlite::types::Value::Text(_)) { return Err(ExportError::Failed); }
                    if previous.as_ref() == Some(&index) { return Err(ExportError::Ambiguous); }
                    previous = Some(index);
                    if let Some(fragment) = row.get::<_, Option<Vec<u8>>>(0)? {
                        let start = fragment.windows(6).position(|w| w == b"#!SILK").unwrap_or(0);
                        bytes.extend_from_slice(&fragment[start..]);
                    }
                }
                if bytes.is_empty() { continue; }
                if !bytes.starts_with(b"#!SILK_V3") { return Err(ExportError::Failed); }
                if found.as_ref().is_some_and(|previous| *previous != bytes) { return Err(ExportError::Ambiguous); }
                found = Some(bytes);
            }
            (found.ok_or(ExportError::Missing)?, "voices", "silk")
        }
        MediaKind::Video => {
            let digest = target.md5.ok_or(ExportError::Unsupported)?;
            let name = lookup_name(aux.get("hardlink/hardlink.db"), "video_hardlink_info_v4", digest)?
                .unwrap_or_else(|| format!("{digest}.mp4"));
            let paths = locate(&ctx.account_dir.join("msg/video"), &name, 3)?;
            let bytes = read_unique(&ctx.account_dir, paths)?;
            if bytes.starts_with(&super::MAGIC_V1) || bytes.starts_with(&super::MAGIC_V2) {
                return Err(ExportError::Unsupported);
            }
            if bytes.get(4..8) != Some(b"ftyp") || !format!("{:x}", md5::Md5::digest(&bytes)).eq_ignore_ascii_case(digest) {
                return Err(ExportError::Failed);
            }
            (bytes, "videos", "mp4")
        }
        _ => return Err(ExportError::Unsupported),
    };
    if bytes.is_empty() { return Err(ExportError::Failed); }
    let name = format!("{:x}.{ext}", Sha256::digest(&bytes));
    prepare_out(&ctx.export_dir, target.talker, dir, &name, &bytes, true).map_err(|_| ExportError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ExportCtx {
        let root = std::env::temp_dir().join(format!("weflow-target-{}-{}", std::process::id(), rand::random::<u64>()));
        ExportCtx { account_dir: root.join("account"), export_dir: root.join("export"), media_keys: None }
    }

    #[test]
    fn voice_chunks_are_ordered_and_conflicting_sources_fail() {
        let ctx = context();
        let voice = || {
            let c = Connection::open_in_memory().unwrap();
            c.execute_batch("CREATE TABLE VoiceInfo(svr_id INTEGER, data_index TEXT, voice_data BLOB)").unwrap();
            c
        };
        let c = voice();
        c.execute("INSERT INTO VoiceInfo VALUES (7, 1, ?1)", [b"tail".as_slice()]).unwrap();
        c.execute("INSERT INTO VoiceInfo VALUES (7, 0, ?1)", [b"#!SILK_V3head".as_slice()]).unwrap();
        let mut aux = HashMap::from([("first".into(), c)]);
        let target = || Target { talker: "room", kind: MediaKind::Voice, md5: None, server_id: 7 };
        let result = export_with(&ctx, &aux, target()).unwrap().publish().unwrap();
        assert_eq!(std::fs::read(result.local_path).unwrap(), b"#!SILK_V3headtail");
        let c = voice();
        c.execute("INSERT INTO VoiceInfo VALUES (7, 0, ?1)", [b"#!SILK_V3different".as_slice()]).unwrap();
        aux.insert("second".into(), c);
        assert!(matches!(export_with(&ctx, &aux, target()), Err(ExportError::Ambiguous)));
        aux.get("second").unwrap().execute("UPDATE VoiceInfo SET voice_data=?1", [b"invalid".as_slice()]).unwrap();
        assert!(matches!(export_with(&ctx, &aux, target()), Err(ExportError::Failed)));
        std::fs::remove_dir_all(ctx.export_dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn local_video_integrity_external_media_and_source_ambiguity() {
        let ctx = context();
        let bytes = b"\x00\x00\x00\x18ftypisomvideo";
        let digest = format!("{:x}", md5::Md5::digest(bytes));
        let source = ctx.account_dir.join("msg/video/first");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(format!("{digest}.mp4")), bytes).unwrap();
        let target = || Target { talker: "room", kind: MediaKind::Video, md5: Some(&digest), server_id: 7 };
        let aux = HashMap::new();
        let result = export_with(&ctx, &aux, target()).unwrap().publish().unwrap();
        assert_eq!(std::fs::read(result.local_path).unwrap(), bytes);
        let other = ctx.account_dir.join("msg/video/second");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(format!("{digest}.mp4")), b"different").unwrap();
        assert!(matches!(export_with(&ctx, &aux, target()), Err(ExportError::Ambiguous)));
        assert!(matches!(export_with(&ctx, &aux, Target { kind: MediaKind::Emoji, ..target() }), Err(ExportError::Unsupported)));
        assert!(matches!(export_with(&ctx, &aux, Target { kind: MediaKind::File, ..target() }), Err(ExportError::Unsupported)));
        std::fs::remove_dir_all(ctx.export_dir.parent().unwrap()).unwrap();
    }
}
