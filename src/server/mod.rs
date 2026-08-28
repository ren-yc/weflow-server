//! HTTP layer: axum router with WeFlow-compatible endpoints (WeChat flavor),
//! client-driven account registration, token auth, SSE push and media serving.
//!
//! Endpoint shapes follow WeFlow docs/HTTP-API.md; the accounts/sync endpoints
//! follow qqflow-server conventions. Default port 5033 (WeFlow 5031 /
//! qqflow-server 5032).

pub mod auth;
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
///
/// The SSE event bus and replay history deliberately live on [`AppState`], not
/// here (qqflow-server parity): they must outlive any individual registration
/// so `/api/v1/push/messages` can be subscribed before the first account
/// exists, and so re-registering a corrected account does not orphan the
/// clients already streaming.
pub struct AccountHandle {
    pub info: AccountInfo,
    pub status: AtomicU8, // AccountStatus as u8
    /// Last initialization failure reason (None while healthy).
    pub error: Mutex<Option<String>>,
    pub store: Arc<RwLock<Store>>,
    pub sync: Arc<Mutex<AccountSync>>,
    /// Precomputed image keys (V2/legacy dat decryption)
    pub media_keys: Option<crate::keystore::ImageKeys>,
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

/// Resolve the base URL used in exported-media links.
///
/// `--base-url` overrides everything verbatim; otherwise it is derived from
/// `--host`/`--port`. Bind-all addresses (`0.0.0.0` / `::`) are not reachable
/// as URLs, so they fall back to 127.0.0.1 with a warning — LAN clients must
/// pass `--base-url` explicitly. IPv6 hosts are bracketed: `[::1]:5033`.
pub fn derive_base_url(host: &str, port: u16, override_url: Option<&str>) -> String {
    if let Some(url) = override_url {
        return url.trim_end_matches('/').to_string();
    }
    let host = match host {
        "0.0.0.0" | "::" => {
            tracing::warn!(
                "[init] 绑定地址 {host} 不可作为 URL，媒体链接回退 127.0.0.1；局域网客户端请用 --base-url 显式指定"
            );
            "127.0.0.1".to_string()
        }
        h => h.to_string(),
    };
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Shared application state.
pub struct AppState {
    pub cfg: Config,
    pub token: String,
    /// Base URL for exported-media links, resolved once at startup by
    /// [`derive_base_url`] (never re-derived per request).
    pub base_url: String,
    pub accounts: Mutex<HashMap<String, Arc<AccountHandle>>>,
    /// Accounts found by the startup platform scan, for discovery only.
    ///
    /// Reported by `/health` as `awaiting_key` when not yet registered, so a
    /// client can see which accounts exist before registering any. These
    /// deliberately never enter `accounts` and never gate readiness — see
    /// [`AppState::set_discovered`].
    pub discovered: Mutex<Vec<AccountInfo>>,
    pub shutdown: tokio::sync::watch::Sender<bool>,
    /// Process-wide SSE event bus (qqflow-server parity). Global rather than
    /// per-account so that `/api/v1/push/messages` needs no ready account to
    /// subscribe (clients connect at startup and receive events once an
    /// account finishes indexing), and so replacing an `error` account keeps
    /// existing subscribers attached to the same sender.
    pub events: broadcast::Sender<Event>,
    /// SSE replay history for Last-Event-ID (1000 items / 10 min TTL).
    /// Global for the same reason as `events` — and necessarily so: the frame
    /// `id` is a bus-level monotonic sequence, which a per-account buffer
    /// could not keep consistent across registrations.
    pub history: Arc<Mutex<HistoryBuf>>,
}

impl AppState {
    /// Bus capacity: one slow subscriber lagging past this many events gets a
    /// `sync` re-baseline frame rather than silently missing messages.
    pub const EVENT_BUS_CAPACITY: usize = 1024;

    /// Build state with a fresh global event bus and empty replay history.
    pub fn new(
        cfg: Config,
        token: String,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        let base_url = derive_base_url(&cfg.host, cfg.port, cfg.base_url.as_deref());
        AppState {
            cfg,
            token,
            base_url,
            accounts: Mutex::new(HashMap::new()),
            discovered: Mutex::new(Vec::new()),
            shutdown,
            events: broadcast::channel(Self::EVENT_BUS_CAPACITY).0,
            history: Arc::new(Mutex::new(HistoryBuf::default())),
        }
    }

    /// Record the startup scan results (discovery only — no keys, no build).
    ///
    /// Kept out of `accounts` on purpose: readiness must consider ONLY
    /// registered accounts. Folding scanned-but-unregistered accounts into the
    /// readiness set would pin `/health` at `starting` forever whenever a
    /// client never registers one of them.
    pub fn set_discovered(&self, found: Vec<AccountInfo>) {
        *self.discovered.lock() = found;
    }

    /// Per-account views for `/health`: every registered account, plus each
    /// discovered-but-unregistered one as `awaiting_key`. Sorted by wxid.
    ///
    /// The second return value is the readiness flag, computed over the
    /// registered accounts alone (see [`AppState::set_discovered`]).
    pub fn account_views(&self) -> (Vec<AccountStateView>, bool) {
        let mut views: Vec<AccountStateView> = {
            let accounts = self.accounts.lock();
            accounts
                .values()
                .map(|h| AccountStateView {
                    wxid: h.info.wxid.clone(),
                    state: h.status(),
                    message_count: h.store.read().total_messages(),
                    error: h.error.lock().clone(),
                })
                .collect()
        };
        // Readiness over registered accounts only, BEFORE the discovered
        // entries are appended.
        let all_ready = !views.is_empty() && views.iter().all(|v| v.state.is_ready());

        let registered: std::collections::HashSet<String> =
            views.iter().map(|v| v.wxid.clone()).collect();
        for info in self.discovered.lock().iter() {
            if !registered.contains(&info.wxid) {
                views.push(AccountStateView {
                    wxid: info.wxid.clone(),
                    state: AccountStatus::AwaitingKey,
                    message_count: 0,
                    error: None,
                });
            }
        }
        views.sort_by(|a, b| a.wxid.cmp(&b.wxid));
        (views, all_ready)
    }
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
        .route("/api/v1/sync", axum::routing::get(sync::handler).post(sync::handler))
        .route(
            "/api/v1/sns/timeline",
            axum::routing::get(sns::timeline).post(sns::timeline),
        )
        .route(
            "/api/v1/sns/usernames",
            axum::routing::get(sns::usernames).post(sns::usernames),
        )
        .route("/api/v1/sns/stats", axum::routing::get(sns::stats).post(sns::stats))
        .route("/api/v1/sns/export", axum::routing::get(sns::export).post(sns::export))
        .route(
            "/api/v1/sns/export/stats",
            axum::routing::get(sns::export_stats).post(sns::export_stats),
        )
        .route(
            "/api/v1/sns/media/proxy",
            axum::routing::get(sns::media_proxy).post(sns::media_proxy),
        )
        .with_state(state)
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
            // Accept either the account root (which contains `db_storage`) or
            // the storage dir itself. A path that is neither passes through
            // unchanged so the `is_dir` check below reports it against the
            // value the client actually sent.
            let storage = if p.join("db_storage").is_dir() {
                p.join("db_storage")
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
    // The watermark baseline is published on the global bus, so the task needs
    // the state (not just the handle) to reach it.
    let state2 = state.clone();
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
                // Global bus: clients already streaming (possibly since before
                // this account existed) get the watermark baseline here.
                let _ = state2.events.send(crate::sync::Event::Sync(wms));
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
    // Publish onto the process-wide bus (never a fresh per-account channel):
    // subscribers attached before this registration — including ones that
    // connected with zero accounts, or before a corrected re-registration —
    // must keep receiving events without reconnecting.
    let sync = Arc::new(Mutex::new(AccountSync::with_channel(
        &info.wxid,
        &info.db_storage,
        keys,
        store.clone(),
        state.events.clone(),
    )));
    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(1), // indexing
        error: Mutex::new(None),
        store,
        sync,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<AppState> {
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            log: "info".into(),
            watch_debounce_ms: 10,
            watch_fallback_ms: 0,
            media_export_dir: std::path::PathBuf::from("target/test-tmp/state"),
            base_url: None,
            data_dir: std::path::PathBuf::from("target/test-tmp/state-data"),
            show_token: false,
        };
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Arc::new(AppState::new(cfg, "0123456789abcdef".into(), shutdown))
    }

    fn info(wxid: &str) -> AccountInfo {
        AccountInfo {
            wxid: wxid.into(),
            dir: std::path::PathBuf::from("/nonexistent").join(wxid),
            db_storage: std::path::PathBuf::from("/nonexistent").join(wxid).join("db_storage"),
            session_db: None,
        }
    }

    /// Discovered-but-unregistered accounts are reported, never gate readiness.
    ///
    /// Guards the (ii) choice: folding them into the readiness set would pin
    /// `/health` at `starting` forever when a client never registers one.
    #[test]
    fn discovered_accounts_are_reported_but_never_gate_readiness() {
        let state = test_state();
        state.set_discovered(vec![info("wxid_scanned")]);

        // Nothing registered yet: listed as awaiting_key, and NOT ready
        // (there is no registered account at all).
        let (views, ready) = state.account_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].wxid, "wxid_scanned");
        assert_eq!(views[0].state, AccountStatus::AwaitingKey);
        assert_eq!(views[0].message_count, 0);
        assert!(!ready, "no registered account means not ready");

        // Register a DIFFERENT account and mark it ready. The still-unregistered
        // scanned account must not hold readiness back.
        let (handle, is_new) = register_account(
            &state,
            info("wxid_registered"),
            crate::keystore::KeyMap::default(),
            None,
        );
        assert!(is_new);
        handle.set_status(AccountStatus::Ready);

        let (views, ready) = state.account_views();
        assert!(ready, "an unregistered scanned account must not block readiness");
        assert_eq!(views.len(), 2, "both are reported");
        // Sorted by wxid: registered < scanned.
        assert_eq!(views[0].wxid, "wxid_registered");
        assert_eq!(views[0].state, AccountStatus::Ready);
        assert_eq!(views[1].wxid, "wxid_scanned");
        assert_eq!(views[1].state, AccountStatus::AwaitingKey);
    }

    /// A registered account that is still indexing DOES gate readiness.
    #[test]
    fn registered_not_ready_account_gates_readiness() {
        let state = test_state();
        let (a, _) = register_account(&state, info("wxid_a"), crate::keystore::KeyMap::default(), None);
        let (b, _) = register_account(&state, info("wxid_b"), crate::keystore::KeyMap::default(), None);
        a.set_status(AccountStatus::Ready);
        b.set_status(AccountStatus::Indexing);
        assert!(!state.account_views().1, "indexing account blocks readiness");
        b.set_status(AccountStatus::Ready);
        assert!(state.account_views().1, "all registered ready -> ready");
    }

    /// Registering a scanned account replaces its discovery entry rather than
    /// listing the same wxid twice.
    #[test]
    fn registering_a_discovered_account_does_not_duplicate_it() {
        let state = test_state();
        state.set_discovered(vec![info("wxid_dup")]);
        let (h, _) = register_account(&state, info("wxid_dup"), crate::keystore::KeyMap::default(), None);
        h.set_status(AccountStatus::Ready);
        let (views, ready) = state.account_views();
        assert_eq!(views.len(), 1, "wxid must not appear twice");
        assert_eq!(views[0].state, AccountStatus::Ready);
        assert!(ready);
    }

    #[test]
    fn base_url_derivation() {
        assert_eq!(
            derive_base_url("127.0.0.1", 5033, None),
            "http://127.0.0.1:5033"
        );
        // A real --host must be honoured (the old hardcoded 127.0.0.1 ignored it).
        assert_eq!(
            derive_base_url("192.168.1.10", 5033, None),
            "http://192.168.1.10:5033"
        );
        // Bind-all addresses are not reachable as URLs -> 127.0.0.1.
        assert_eq!(derive_base_url("0.0.0.0", 5033, None), "http://127.0.0.1:5033");
        assert_eq!(derive_base_url("::", 5033, None), "http://127.0.0.1:5033");
        // IPv6 hosts get brackets.
        assert_eq!(derive_base_url("::1", 5033, None), "http://[::1]:5033");
        // --base-url overrides everything, minus any trailing slash.
        assert_eq!(
            derive_base_url("0.0.0.0", 5033, Some("http://192.168.1.10:5033")),
            "http://192.168.1.10:5033"
        );
        assert_eq!(
            derive_base_url("0.0.0.0", 5033, Some("http://example.test/")),
            "http://example.test"
        );
    }
}
