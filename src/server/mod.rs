//! HTTP layer: axum router with WeFlow-compatible endpoints (WeChat flavor),
//! client-driven account registration, token auth, SSE push and media serving.
//!
//! Endpoint shapes follow WeFlow docs/HTTP-API.md; the accounts/sync endpoints
//! follow qqflow-server conventions. Default port 5033 (WeFlow 5031 /
//! qqflow-server 5032).

pub mod error;
pub mod handlers;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::db::scan::AccountInfo;
use crate::server::error::ApiError;
use crate::store::Store;
use crate::sync::{watch::WatchConfig, AccountSync, Event};

/// Serialized account status exposed via /health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    AwaitingKey,
    Indexing,
    Ready,
    Error,
}

impl AccountStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// One registered account's runtime state.
pub struct AccountHandle {
    pub info: AccountInfo,
    pub status: AtomicU8, // AccountStatus as u8
    /// Last initialization failure reason (None while healthy).
    pub error: Mutex<Option<String>>,
    pub store: Arc<RwLock<Store>>,
    pub events: broadcast::Sender<Event>,
    pub sync: Arc<Mutex<AccountSync>>,
    /// Precomputed image keys (V2/legacy dat decryption)
    pub media_keys: Option<crate::keystore::ImageKeys>,
    /// SSE event history for Last-Event-ID replay (1000 items / 10 min TTL).
    pub history: Arc<Mutex<HistoryBuf>>,
    /// Watch task handle (started on registration; dropped on re-register).
    pub watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// One buffered SSE event (WeFlow contract: replay cap 1000, TTL 10 min).
pub struct HistoryItem {
    pub id: u64,
    pub at: std::time::Instant,
    pub name: &'static str,
    pub payload: serde_json::Value,
}

#[derive(Default)]
pub struct HistoryBuf {
    items: std::collections::VecDeque<HistoryItem>,
    last_id: u64,
}

impl HistoryBuf {
    pub const MAX: usize = 1000;
    pub const TTL: std::time::Duration = std::time::Duration::from_secs(600);

    /// Append an event and return its id (monotonic).
    pub fn append(&mut self, name: &'static str, payload: serde_json::Value) -> u64 {
        self.last_id += 1;
        self.items.push_back(HistoryItem {
            id: self.last_id,
            at: std::time::Instant::now(),
            name,
            payload,
        });
        while self.items.len() > Self::MAX {
            self.items.pop_front();
        }
        self.last_id
    }

    /// Events with id > `since`, still within the TTL window.
    pub fn replay_since(&self, since: u64) -> Vec<(u64, &'static str, serde_json::Value)> {
        let now = std::time::Instant::now();
        self.items
            .iter()
            .filter(|i| i.id > since && now.duration_since(i.at) < Self::TTL)
            .map(|i| (i.id, i.name, i.payload.clone()))
            .collect()
    }
}

impl AccountHandle {
    pub fn status(&self) -> AccountStatus {
        match self.status.load(Ordering::Relaxed) {
            1 => AccountStatus::Indexing,
            2 => AccountStatus::Ready,
            3 => AccountStatus::Error,
            _ => AccountStatus::AwaitingKey,
        }
    }
    pub fn set_status(&self, s: AccountStatus) {
        let v = match s {
            AccountStatus::AwaitingKey => 0,
            AccountStatus::Indexing => 1,
            AccountStatus::Ready => 2,
            AccountStatus::Error => 3,
        };
        self.status.store(v, Ordering::Relaxed);
    }
}

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub token: String,
    pub accounts: Mutex<HashMap<String, Arc<AccountHandle>>>,
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountStateView {
    pub wxid: String,
    pub state: AccountStatus,
    pub message_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    use handlers::*;
    Router::new()
        .route("/health", axum::routing::get(health::handler).post(health::handler))
        .route("/api/v1/health", axum::routing::get(health::handler).post(health::handler))
        .route("/api/v1/accounts", axum::routing::post(accounts::handler))
        .route("/api/v1/messages", axum::routing::get(messages::handler).post(messages::handler))
        .route("/api/v1/sessions", axum::routing::get(sessions::handler).post(sessions::handler))
        .route(
            "/api/v1/sessions/{id}/messages",
            axum::routing::get(chatlab_pull::handler),
        )
        .route("/api/v1/contacts", axum::routing::get(contacts::handler).post(contacts::handler))
        .route(
            "/api/v1/group-members",
            axum::routing::get(group_members::handler).post(group_members::handler),
        )
        .route(
            "/api/v1/media/{talker}/{media_type}/{file}",
            axum::routing::get(media::handler).post(media::handler),
        )
        .route(
            "/api/v1/push/messages",
            axum::routing::get(push_events::handler).post(push_events::handler),
        )
        .route("/api/v1/sync", axum::routing::get(sync::handler).post(sync::handler))        .route("/api/v1/sns/timeline", axum::routing::get(sns::timeline).post(sns::timeline))
        .route("/api/v1/sns/usernames", axum::routing::get(sns::usernames).post(sns::usernames))
        .route("/api/v1/sns/stats", axum::routing::get(sns::stats).post(sns::stats))        .route("/api/v1/sns/export", axum::routing::get(sns::export).post(sns::export))
        .route("/api/v1/sns/export/stats", axum::routing::get(sns::export_stats).post(sns::export_stats))
        .route("/api/v1/sns/media/proxy", axum::routing::get(sns::media_proxy).post(sns::media_proxy))
        .with_state(state)
}

const TOKEN_SERVICE: &str = "weflow-server";
const TOKEN_USER: &str = "http-api-token";

/// Access token kept in the OS credential store (Windows Credential
/// Manager / macOS Keychain / Linux Secret Service). Never written to a
/// token file.
///
/// The token is printed to the log **only when it is first generated**
/// (or when the credential store is unavailable and the token is
/// per-session). On subsequent launches it is fetched silently; use the
/// `--show-token` flag to retrieve it on demand.
pub fn load_token() -> Result<String> {
    let entry = keyring::Entry::new(TOKEN_SERVICE, TOKEN_USER)
        .map_err(|e| anyhow::anyhow!("凭据库初始化失败: {e}"))?;
    match entry.get_password() {
        Ok(t) if t.len() >= 16 => Ok(t),
        Ok(_) => {
            // short/corrupt value: regenerate and overwrite
            let t = new_token();
            entry
                .set_password(&t)
                .map_err(|e| anyhow::anyhow!("凭据库写入失败: {e}"))?;
            tracing::info!("[init] 生成新 API token: {t}（已存入系统凭据库）");
            Ok(t)
        }
        Err(keyring::Error::NoEntry) => {
            let t = new_token();
            entry
                .set_password(&t)
                .map_err(|e| anyhow::anyhow!("凭据库写入失败: {e}"))?;
            tracing::info!("[init] 生成新 API token: {t}（已存入系统凭据库）");
            Ok(t)
        }
        Err(e) => {
            // no credential store available: per-session token, log only
            let t = new_token();
            tracing::warn!(
                "[init] 凭据库不可用 ({e})；API token 为会话级（重启后变化）: {t}"
            );
            Ok(t)
        }
    }
}

/// Read the stored token without generating one; `None` when none exists.
/// Used by `--show-token`.
pub fn show_token() -> Result<Option<String>> {
    let entry = keyring::Entry::new(TOKEN_SERVICE, TOKEN_USER)
        .map_err(|e| anyhow::anyhow!("凭据库初始化失败: {e}"))?;
    match entry.get_password() {
        Ok(t) if !t.is_empty() => Ok(Some(t)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("凭据库读取失败: {e}")),
    }
}

fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Merge query params and JSON body into one param map (body wins).
pub fn merge_params(
    query: &axum::extract::Query<HashMap<String, String>>,
    body: Option<serde_json::Value>,
) -> HashMap<String, String> {
    let mut out = query.0.clone();
    if let Some(body) = body
        && let Some(map) = body.as_object() {
            for (k, v) in map {
                out.insert(
                    k.clone(),
                    match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    },
                );
            }
        }
    out
}

/// Parse `1/true/yes` style boolean params (query strings and JSON alike).
pub fn flex_bool(params: &HashMap<String, String>, key: &str) -> bool {
    params
        .get(key)
        .map(|v| {
            let v = v.to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

pub fn parse_limit(params: &HashMap<String, String>, key: &str, default: usize, max: usize) -> usize {
    params
        .get(key)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

pub fn parse_offset(params: &HashMap<String, String>, key: &str) -> usize {
    params.get(key).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0)
}

/// Resolve a "YYYYMMDD" or unix-seconds timestamp to seconds.
pub fn parse_time_bound(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i64 = s[0..4].parse().ok()?;
        let m: i64 = s[4..6].parse().ok()?;
        let d: i64 = s[6..8].parse().ok()?;
        let naive = chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)?;
        let dt = naive
            .and_hms_opt(0, 0, 0)?
            .and_utc();
        return Some(dt.timestamp());
    }
    s.parse::<i64>().ok()
}

/// Register an account and schedule its async build/watcher.
///
/// Shared by the HTTP handler.
/// Keys are held in memory only; nothing is persisted to disk.
pub async fn start_account(
    state: Arc<AppState>,
    body: crate::server::handlers::accounts::AccountBody,
) -> Result<Arc<AccountHandle>, ApiError> {
    use crate::db::scan;
    use crate::keystore::parse_db_key;

    let wxid = body
        .wxid
        .clone()
        .ok_or_else(|| ApiError::bad_request("wxid required"))?;

    let info = match &body.db_path {
        Some(p) if !p.is_empty() => {
            let p = std::path::PathBuf::from(p);
            let storage = if p.join("db_storage").is_dir() {
                p.join("db_storage")
            } else if p.is_dir() {
                p.clone()
            } else {
                p.clone()
            };
            Some(scan::AccountInfo {
                wxid: wxid.clone(),
                dir: p.clone(),
                db_storage: storage,
                session_db: scan::find_session_db(&p),
            })
        }
        _ => {
            let candidates = scan::scan_all(&scan::default_roots());
            candidates.into_iter().find(|a| a.wxid == wxid)
        }
    };
    let Some(info) = info else {
        return Err(ApiError::bad_request(format!(
            "account '{wxid}' not found; check xwechat_files or pass db_path"
        )));
    };
    if !info.db_storage.is_dir() {
        return Err(ApiError::bad_request(format!(
            "db_storage not found at {}",
            info.db_storage.display()
        )));
    }

    let session_path = info
        .session_db
        .clone()
        .unwrap_or_else(|| info.db_storage.join("session/session.db"));
    let key_map = crate::keystore::KeyMap::from_parts(
        body.key
            .as_deref()
            .map(parse_db_key)
            .transpose()
            .map_err(|e| ApiError::bad_request(e.to_string()))?,
        body.keys.clone(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if key_map.is_empty() {
        return Err(ApiError::bad_request(
            "provide `key` (uniform) or `keys` (per-db map)",
        ));
    }

    let session_rel = info
        .session_db
        .as_ref()
        .and_then(|s| s.strip_prefix(&info.db_storage).ok())
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| "session/session.db".into());
    if let Some(sk) = key_map.key_for(&session_rel) {
        if let Ok(bytes) = std::fs::read(&session_path) {
            let head = bytes.len().min(4096);
            if !crate::db::wcdb::verify_page1(&sk.0, &bytes[..head]) {
                return Err(ApiError::bad_request(
                    "session.db key failed page-1 HMAC validation",
                ));
            }
        }
    } else {
        return Err(ApiError::bad_request(format!(
            "no key for the session database ('{session_rel}'); provide it in `keys`"
        )));
    }

    let media_keys = if let (Some(aes), Some(xor)) = (&body.img_aes_key, &body.img_xor_key) {
        Some(
            crate::keystore::ImageKeys::from_parts(aes, xor)
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
        )
    } else {
        body.img_code
            .as_ref()
            .map(|code| crate::keystore::ImageKeys::from_img_code(&crate::keystore::ImgCode(code.clone()), &info.wxid))
    };

    let (handle, is_new) = register_account(&state, info.clone(), key_map, media_keys);
    if !is_new {
        // A ready/indexing account was re-registered: reuse it, no rebuild.
        return Ok(handle);
    }

    // async build: full_sync then mark ready and spawn the watcher
    let handle2 = handle.clone();
    let handle3 = handle.clone();
    let watch_cfg = watch_config(&state.cfg);
    let shutdown_rx = state.shutdown.subscribe();
    tokio::spawn(async move {
        let result =
            tokio::task::spawn_blocking(move || handle3.sync.lock().full_sync()).await;
        match result {
            Ok(Ok(n)) => {
                handle2.set_status(crate::server::AccountStatus::Ready);
                *handle2.error.lock() = None;
                let msg_total = {
                    let guard = handle2.store.read();
                    guard.total_messages()
                };
                tracing::info!(
                    "[init] 账号 {} 索引完成: {} 个数据库, 共 {} 条消息",
                    handle2.info.wxid,
                    n,
                    msg_total
                );
                let wms: Vec<(String, crate::store::Watermark)> = {
                    let guard = handle2.store.read();
                    guard.watermarks.clone().into_iter().collect()
                };
                let _ = handle2.events.send(crate::sync::Event::Sync(wms));
                let acct = handle2.sync.clone();
                let dir = handle2.info.db_storage.clone();
                let h = tokio::spawn(async move {
                    if let Err(e) =
                        crate::sync::watch::spawn(acct, dir, watch_cfg, shutdown_rx).await
                    {
                        tracing::warn!("watcher exited: {e:#}");
                    }
                });
                *handle2.watcher.lock() = Some(h);
            }
            Ok(Err(e)) => {
                handle2.set_status(crate::server::AccountStatus::Error);
                *handle2.error.lock() = Some(format!("{e:#}"));
                tracing::warn!("[init] 账号 {} 初始化失败（重新注册可恢复）: {e:#}", handle2.info.wxid);
            }
            Err(e) => {
                handle2.set_status(crate::server::AccountStatus::Error);
                *handle2.error.lock() = Some(format!("index task panicked: {e}"));
                tracing::error!("[init] 账号 {} 初始化任务异常: {e}", handle2.info.wxid);
            }
        }
    });

    Ok(handle)
}

/// Register the account handle in the registry.
///
/// Idempotent (qqflow-server parity): if a handle for the same `wxid`
/// already exists and is not in the `error` state, the existing handle is
/// returned (`is_new == false`) — no rebuild, no watcher aborted. Only
/// `error` (or awaiting-key) accounts are replaced, giving a corrected
/// registration a clean recovery path.
pub fn register_account(
    state: &Arc<AppState>,
    info: AccountInfo,
    keys: crate::keystore::KeyMap,
    media_keys: Option<crate::keystore::ImageKeys>,
) -> (Arc<AccountHandle>, bool) {
    // Idempotent guard before allocating anything: re-registration of an
    // already ready/indexing account returns the live handle as-is.
    if let Some(old) = { state.accounts.lock().get(&info.wxid).cloned() } {
        match old.status() {
            AccountStatus::Ready | AccountStatus::Indexing => return (old, false),
            _ => {} // awaiting_key / error -> fall through to replace below
        }
    }

    let store = Arc::new(RwLock::new(Store::default()));
    let (events, _) = broadcast::channel(1024);
    let sync = Arc::new(Mutex::new(AccountSync::with_channel(
        &info.wxid,
        &info.db_storage,
        keys,
        store.clone(),
        events.clone(),
    )));
    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(1), // indexing
        error: Mutex::new(None),
        store,
        events,
        sync,
        history: Arc::new(Mutex::new(HistoryBuf::default())),
        media_keys,
        watcher: Mutex::new(None),
    });

    let mut accounts = state.accounts.lock();
    if let Some(old) = accounts.insert(handle.info.wxid.clone(), handle.clone())
        && let Some(task) = old.watcher.lock().take() {
            task.abort();
        }
    (handle, true)
}

/// Build the watch config from CLI.
pub fn watch_config(cfg: &Config) -> WatchConfig {
    WatchConfig {
        debounce: std::time::Duration::from_millis(cfg.watch_debounce_ms),
        fallback: (cfg.watch_fallback_ms > 0).then(|| std::time::Duration::from_millis(cfg.watch_fallback_ms)),
    }
}