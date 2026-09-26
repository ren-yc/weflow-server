//! GET/POST /api/v1/media/{talker}/{media_type}/{file} — serve exported media
//! from the export directory with traversal protection (WeFlow contract).
//! GET/POST /api/v1/media/{id} — the same files, addressed by name alone.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio_util::io::ReaderStream;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, require_auth};
use crate::server::AppState;

const ALLOWED_TYPES: [&str; 4] = ["images", "voices", "videos", "emojis"];

pub async fn handler(
    State(state): State<Arc<AppState>>,
    AxumPath((talker, media_type, file)): AxumPath<(String, String, String)>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Response> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;

    if !ALLOWED_TYPES.contains(&media_type.as_str()) {
        return Err(ApiError::bad_request("unknown media type"));
    }
    // One shared rule for every path component in this service (`pathsafe`):
    // it also covers the cases a separator-only filter misses — a trailing dot
    // or space, which Win32 strips, and `:`, which names an NTFS alternate data
    // stream without carrying a separator at all.
    if ![talker.as_str(), media_type.as_str(), file.as_str()]
        .iter()
        .all(|s| crate::pathsafe::safe_segment(s))
    {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    // canonicalize is real file IO: it must run on the blocking pool, never on a
    // tokio worker, or concurrent media reads starve every other request
    // (including the SSE keep-alives).
    let root_dir = state.cfg.media_export_dir.clone();
    let (canonical, canonical_root) = tokio::task::spawn_blocking(move || {
        let joined = root_dir.join(&talker).join(&media_type).join(&file);
        (joined.canonicalize(), root_dir.canonicalize())
    })
    .await
    .map_err(|e| ApiError::internal(format!("media path resolution task failed: {e}")))?;

    // Same envelope as `serve_file`'s 404 below: "path does not exist" and
    // "path exists but cannot be opened" are one failure mode to the caller,
    // so they must not come back as two different response shapes.
    let canonical = canonical.map_err(|_| ApiError::not_found("media not found"))?;
    // Fails closed on an unresolvable root: comparing a verbatim-prefixed
    // canonical path against a raw one would never match anyway, so this must
    // not fall back to the non-canonical root.
    let canonical_root = canonical_root.map_err(|_| ApiError::not_found("media not found"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    serve_file(&canonical).await
}

/// 单段路由：`GET|POST /api/v1/media/{id}`。
///
/// `{id}` 是**导出文件名**（形如 `<md5>.<ext>`），不是文件系统路径。它由内容摘要
/// 派生、全局唯一，因此可以在导出根下按名解析——不必维护一张「id → 路径」的登记表，
/// 而登记表必然会与磁盘漂移：导出被清理之后登记仍在，于是「出现即保证可取」变成谎话。
///
/// 三段式路由要求调用方知道会话与媒体类型；这条只需要一个名字。
pub async fn handler_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Response> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    // 与三段式共用同一条边界规则（`pathsafe`）：单段路由同样要挡住尾点、尾空格
    // （Win32 会剥掉）与 `:`（NTFS 数据流）——它们都不带路径分隔符，只滤分隔符会漏。
    if !crate::pathsafe::safe_segment(&id) {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    let root_dir = state.cfg.media_export_dir.clone();
    let found = tokio::task::spawn_blocking(move || find_exported(&root_dir, &id))
        .await
        .map_err(|e| ApiError::internal(format!("media path resolution task failed: {e}")))?;
    let found = found.ok_or_else(|| ApiError::not_found("media not found"))?;

    // 与三段式同一套包含性检查。符号链接可以让「文件存在」为真而真实目标在根外，
    // 所以必须规范化后再比前缀；**取不到根就失败**——拿未规范化的根去比永远不相等，
    // 那会把检查变成永假。
    let root_dir = state.cfg.media_export_dir.clone();
    let (canonical, canonical_root) = tokio::task::spawn_blocking(move || {
        (found.canonicalize(), root_dir.canonicalize())
    })
    .await
    .map_err(|e| ApiError::internal(format!("media path resolution task failed: {e}")))?;
    let canonical = canonical.map_err(|_| ApiError::not_found("media not found"))?;
    let canonical_root = canonical_root.map_err(|_| ApiError::not_found("media not found"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    serve_file(&canonical).await
}

/// 在导出根下按文件名查找。布局是 `<root>/<会话>/<类型>/<文件>`，而 `{id}` 不带会话，
/// 所以把已知的四个类型目录逐一代入。只做 `is_file` 判断、不递归：
/// 一次请求最多 (会话数 × 4) 次 stat。
fn find_exported(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    for talker in std::fs::read_dir(root).ok()?.flatten() {
        let dir = talker.path();
        if !dir.is_dir() {
            continue;
        }
        for media_type in ALLOWED_TYPES {
            let candidate = dir.join(media_type).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

async fn serve_file(path: &Path) -> ApiResult<Response> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::not_found("media not found"))?;
    let meta = file
        .metadata()
        .await
        .map_err(|_| ApiError::not_found("media not found"))?;
    let ct = content_type(path);
    let stream = ReaderStream::new(file);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ct.to_string()),
            (header::CONTENT_LENGTH, meta.len().to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("silk") => "audio/x-silk",
        _ => "application/octet-stream",
    }
}
