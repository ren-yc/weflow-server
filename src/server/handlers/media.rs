//! GET /api/v1/media/{id} — 按导出文件名取字节。
//!
//! `{id}` 是**导出文件名**（形如 `<md5>.<ext>`），不是文件系统路径。它由内容摘要派生、全局
//! 唯一，因此可以在导出根下按名解析 —— 不必维护一张「id → 路径」的登记表，而登记表必然会与
//! 磁盘漂移：导出被清理之后登记仍在，于是「出现即保证可取」变成谎话。

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio_util::io::ReaderStream;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::require_auth;
use crate::server::AppState;

/// 参与导出的四个类型目录 ＝ **按名取字节的白名单**：别的目录里就算躺着同名文件也不服务
/// —— 那些位置不是导出管线写出来的，服务它们等于把「导出根」变成「任意文件根」。
///
/// 值来自 `media::export::EXPORT_TYPE_DIRS`（唯一一份）：这里再字面写一遍，就会与「导出实际
/// 写到哪些目录」「拉取面按名查哪些目录」分成三份各自漂移。
const ALLOWED_TYPES: [&str; 4] = crate::media::export::EXPORT_TYPE_DIRS;

pub async fn handler_by_id(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // 鉴权只看查询串：POST body 不是鉴权通道。
    require_auth(&state, &query, &headers)?;
    // 与导出侧共用同一条边界规则（pathsafe）：这条路由同样要挡住尾点、尾空格（Win32 会剥掉）
    // 与冒号（NTFS 数据流）—— 它们都不带路径分隔符，只滤分隔符会漏。
    if !crate::pathsafe::safe_segment(&id) {
        return Err(ApiError::bad_request("path traversal attempt"));
    }

    let root_dir = state.cfg.media_export_dir.clone();
    let found = tokio::task::spawn_blocking(move || find_exported(&root_dir, &id))
        .await
        .map_err(|e| ApiError::internal(format!("media path resolution task failed: {e}")))?;
    // 「没找到」与「同名冲突」对调用方是同一种失败：都给 404 —— 冲突的细节写进日志，
    // 因为把它回给客户端只会泄露「别的会话里有什么」。
    let found = found.ok_or_else(|| ApiError::not_found("media not found"))?;

    // 包含性检查：符号链接可以让「文件存在」为真而真实目标在根外，所以必须规范化后再比前缀；
    // **取不到根就失败**——拿未规范化的根去比永远不相等，那会把检查变成永假。
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

/// 在导出根下按文件名查找。布局是 `<root>/<会话>/<类型>/<文件>`，而 id 不带会话，
/// 所以把已知的四个类型目录逐一代入。只做 `is_file` 判断、不递归：一次请求最多
/// (会话数 × 4) 次 stat。
///
/// **同名多命中**：候选内容**一致**时取排序后的第一个；**不一致**时返回 `None`（调用方给 404）。
/// 为什么不能「取第一个就完事」：按名解析是跨会话的，别的会话里可能躺着一个同名但内容不同的
/// 文件 —— 那时随便挑一个，等于把「出现即可取」变成「出现即可取到某个东西」，而调用方无从察觉。
///
/// 比较顺序与成本：先比 **size** 短路（绝大多数同名异内容在大小上就不同），只有 size 相同才逐字节
/// 读整份文件。读放大 = 候选数 × 文件大小，是**线性**成本 —— 写在这里而不是藏在实现里。
fn find_exported(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    // 目录遍历顺序本身不保证稳定 ⇒ 先排序，「取第一个」才是确定的（否则同一份导出根两次请求
    // 可能命中不同的候选，而两个候选内容一致时看不出问题、不一致时就成了随机 404）。
    let mut talkers: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    talkers.sort();
    // 每个候选**先**规范化并确认仍在导出根内，**再**参与比较：`is_file` 会跟随符号链接，
    // 而链接可以把「根内有个文件」变成「根外有个文件」—— 比较阶段会读它的字节。
    // 取不到根就整体失败（拿未规范化的根去比永远不相等，那会把检查变成永假）。
    let canonical_root = root.canonicalize().ok()?;
    let mut hits: Vec<std::path::PathBuf> = Vec::new();
    for dir in talkers {
        for media_type in ALLOWED_TYPES {
            let candidate = dir.join(media_type).join(name);
            if !candidate.is_file() {
                continue;
            }
            match candidate.canonicalize() {
                Ok(c) if c.starts_with(&canonical_root) => hits.push(c),
                _ => tracing::warn!(
                    "media id {name:?} 的候选 {} 不在导出根内：跳过",
                    candidate.display()
                ),
            }
        }
    }
    let first = hits.first()?.clone();
    if hits.len() > 1 {
        let baseline = std::fs::metadata(&first).map(|m| m.len()).ok()?;
        for other in &hits[1..] {
            let size = std::fs::metadata(other).map(|m| m.len()).ok()?;
            if size != baseline || !same_contents(&first, other) {
                tracing::warn!(
                    "media id {name:?} 在导出根下有多个同名但内容不同的文件：拒绝服务（命中 {} 处）",
                    hits.len()
                );
                return None;
            }
        }
    }
    Some(first)
}

/// 逐字节比较两个文件（只在 size 相同时才被调用）。
fn same_contents(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let (na, nb) = match (fa.read(&mut ba), fb.read(&mut bb)) {
            (Ok(na), Ok(nb)) => (na, nb),
            _ => return false,
        };
        if na != nb || ba[..na] != bb[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
    }
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
