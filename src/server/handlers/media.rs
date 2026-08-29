//! GET/POST /api/v1/media/{talker}/{media_type}/{file} — serve exported media
//! from the export directory with traversal protection (WeFlow contract).

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
