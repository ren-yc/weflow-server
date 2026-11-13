//! Targeted restoration over the full conversation, independent of page size.
use std::{collections::HashMap, sync::Arc};
use axum::{extract::{Query, State}, http::{HeaderMap, StatusCode}, Json};
use crate::server::{AppState, dto::{MediaExportRequest, MediaExportResult}, error::{ApiError, ApiResult}};
use crate::media::{export::ExportCtx, target::{Target, ExportError}};
use super::{require_auth, ready_account};

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<MediaExportResult>> {
    require_auth(&state, &query, &headers)?;
    let request: MediaExportRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("媒体导出请求字段缺失或格式错误"))?;
    let account = ready_account(&state, &query)?;
    restore(state, account, request, || {}).await.map(Json)
}

/// The hook fixes the interleaving after staging and before publication in tests.
#[cfg(feature = "testing")]
pub async fn restore_before_publish(
    state: Arc<AppState>, request: MediaExportRequest, before_publish: impl FnOnce() + Send + 'static,
) -> Result<MediaExportResult, ApiError> {
    let account = ready_account(&state, &HashMap::new())?;
    restore(state, account, request, before_publish).await
}

async fn restore(
    state: Arc<AppState>, account: Arc<crate::server::AccountHandle>, request: MediaExportRequest, before_publish: impl FnOnce() + Send + 'static,
) -> Result<MediaExportResult, ApiError> {
    if request.talker.is_empty() || request.platform_message_id.is_empty() || request.sender.is_empty() {
        return Err(ApiError::bad_request("talker、platformMessageId 和 sender 不能为空"));
    }
    let (hint, server_id) = {
        let store = account.store.read();
        let conv = store.convs.get(&request.talker).ok_or_else(|| ApiError::not_found("目标消息不存在"))?;
        let mut matches = conv.iter().filter(|m| m.server_id.to_string() == request.platform_message_id
            && m.sender_username == request.sender && m.create_time == request.timestamp);
        let target = matches.next().ok_or_else(|| ApiError::not_found("目标消息不存在"))?;
        if matches.next().is_some() { return Err(failure(ExportError::Ambiguous)); }
        (target.parsed.media.clone().ok_or_else(|| failure(ExportError::Unsupported))?, target.server_id)
    };
    let context = ExportCtx { account_dir: account.info.dir.clone(),
        export_dir: state.cfg.media_export_dir.clone(), media_keys: account.media_keys };
    // 记下请求开始时的数据源世代与身份指纹。
    //
    // 只比 `generation` 不够：它只在同步路径的 `sample_source` 里推进，而导出的整个
    // 阻塞任务都持有 `sync` 锁——这期间轮询根本跑不了，计数器必然不动。所以这里
    // 自己采一次指纹（路径 + 创建时间 + salt），发布前再采一次比对。
    let generation = account.generation.load(std::sync::atomic::Ordering::SeqCst);
    let fingerprint = crate::db::source_identity::fingerprint(&account.info.db_storage).ok();
    let sync = account.sync.clone();
    tokio::task::spawn_blocking(move || {
        let result = {
            let sync = sync.lock();
            if sync.is_stopped() { return Err(failure(ExportError::Ambiguous)); }
            sync.export_media_targeted(&context, Target {
                talker: &request.talker, kind: hint.kind, md5: hint.md5.as_deref(), server_id,
            })
        };
        before_publish();
        let accounts = state.accounts.lock();
        if account.is_stopped() || !accounts.values().any(|current| Arc::ptr_eq(current, &account)) {
            return Err(ApiError::conflict("导出期间账号注册发生切换"));
        }
        if account.generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
            return Err(ApiError::conflict("导出期间数据源发生变化"));
        }
        // 指纹比对是这条复查真正起作用的那一半：换库/分片增删都会改变它。
        // 起始采样失败时退回到世代比较（尽力检测，边界写进文档）；起始采到了、
        // 现在采不到，按「无法证明」处理——不发布也不赌。
        if let Some(before) = fingerprint {
            let current = crate::db::source_identity::fingerprint(&account.info.db_storage).ok();
            if current.as_ref() != Some(&before) {
                return Err(ApiError::conflict("导出期间数据源发生变化"));
            }
        }
        let result = result.map_err(failure)?.publish().map_err(|_| failure(ExportError::Failed))?;
        Ok(MediaExportResult { file_name: hint.file_name.clone(), media_id: result.file_name.clone(),
            media_url: format!("/api/v1/media/{}", result.file_name), success: true })
    }).await.map_err(|_| ApiError::internal("媒体导出任务异常"))?

}

fn failure(error: ExportError) -> ApiError {
    let (status, message) = match error {
        ExportError::Missing => (StatusCode::NOT_FOUND, "目标消息或原始媒体不存在"),
        ExportError::Unsupported => (StatusCode::UNPROCESSABLE_ENTITY, "目标消息没有可导出的本地媒体"),
        ExportError::Ambiguous => (StatusCode::CONFLICT, "目标或媒体来源不唯一"),
        ExportError::Failed => (StatusCode::INTERNAL_SERVER_ERROR, "媒体读取、解码或导出失败"),
    };
    ApiError { status, message: message.into(), reason: None }
}
