//! GET/POST /api/v1/media/{talker}/{media_type}/{file} — serve exported media
//! from the export directory with traversal protection (WeFlow contract).

use std::path::{Path, PathBuf};
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
    if talker.is_empty() || media_type.is_empty() || file.is_empty() {
        return Err(ApiError::bad_request("empty path segment"));
    }
    if [talker.as_str(), media_type.as_str(), file.as_str()]
        .iter()
        .any(|s| s.contains('/') || s.contains('\\') || *s == "." || *s == "..")
    {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    let file_path = state
        .cfg
        .media_export_dir
        .join(&talker)
        .join(&media_type)
        .join(&file);

    // canonicalize + prefix check (double traversal protection)
    let root = state
        .cfg
        .media_export_dir
        .canonicalize()
        .unwrap_or_else(|_| state.cfg.media_export_dir.clone());
    // Same envelope as `serve_file`'s 404 below: "path does not exist" and
    // "path exists but cannot be opened" are one failure mode to the caller,
    // so they must not come back as two different response shapes.
    let canonical = file_path
        .canonicalize()
        .map_err(|_| ApiError::not_found("media not found"))?;
    if !canonical.starts_with(&root) {
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

#[allow(dead_code)]
fn _path_buf(s: &str) -> PathBuf {
    PathBuf::from(s)
}
