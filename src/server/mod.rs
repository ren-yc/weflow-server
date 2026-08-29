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

/// One account's state-machine value, exposed via the token-protected
/// `GET /api/v1/accounts` and echoed by the registration endpoint.
///
/// NOT what `/health` reports: that endpoint is unauthenticated and carries the
/// coarser [`AccountPhase`] instead, which has no `AwaitingKey` variant.
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

/// Coarse account phase for the unauthenticated `/health`.
///
/// `/health` needs no token, so it must not reveal which accounts exist on
/// this machine, how many there are, where their databases live, or why one
/// failed. The startup scan seeds one discovery entry per `xwechat_files`
/// profile directory, which makes even the *count* a disclosure. This enum has
/// no `AwaitingKey` variant at all, so leaking discovery results through
/// `/health` is a type error rather than a review item; the detail lives
/// behind the token-protected `GET /api/v1/accounts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPhase {
    /// Nothing is bound (never registered, or deregistered since).
    Unregistered,
    /// The bound account is building its index.
    Indexing,
    /// The bound account is serving.
    Ready,
    /// The bound account failed to initialize; re-registering it recovers.
    Error,
}

impl From<AccountStatus> for AccountPhase {
    fn from(s: AccountStatus) -> Self {
        match s {
            // Unreachable through `bound_account` (the registry never holds an
            // `AwaitingKey` entry — see [`bound_account`]), but mapping it here
            // keeps the "never leak discovery" invariant true by construction.
            AccountStatus::AwaitingKey => Self::Unregistered,
            AccountStatus::Indexing => Self::Indexing,
            AccountStatus::Ready => Self::Ready,
            AccountStatus::Error => Self::Error,
        }
    }
}

/// The one account this server instance is bound to, if any.
///
/// At most one account may be registered at a time. The capability for more
/// never had a user and several subsystems quietly assume a single account:
/// business handlers resolve a default account by scanning the registry (which
/// is unordered, so "the default" was nondeterministic with two ready
/// accounts), the exported-media layout is `<talker>/<kind>/<file>` with no
/// account dimension, the SSE bus is process-wide and its `wxid` field is
/// documented as a hint that does not partition the stream, and WeChat 4.x
/// only ever has one logged-in account whose databases actually change.
///
/// Every entry in `accounts` is a binding: [`register_account`] inserts with
/// `indexing`, and discovery results live in `discovered` instead — so an
/// `AwaitingKey` status never appears here and this returns the sole value.
pub fn bound_account(
    accounts: &HashMap<String, Arc<AccountHandle>>,
) -> Option<Arc<AccountHandle>> {
    accounts.values().next().cloned()
}

/// Outcome of claiming the single account binding — see [`register_account`].
pub enum BindOutcome {
    /// The binding is now held by this wxid; the caller must run the build.
    Bound(Arc<AccountHandle>),
    /// This same wxid is already `ready`/`indexing`; nothing changed and the
    /// live handle is returned as-is (no rebuild, no watcher aborted).
    Existing(Arc<AccountHandle>),
    /// A different wxid holds the binding; nothing changed.
    Occupied { wxid: String, status: AccountStatus },
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
    /// Retirement flag, shared with this handle's [`AccountSync`].
    ///
    /// An `Arc` rather than a plain flag on purpose: deregistration must be
    /// able to set it WITHOUT taking `sync`'s mutex, which a `full_sync` on a
    /// real account can hold for minutes.
    pub stopped: Arc<std::sync::atomic::AtomicBool>,
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
    /// Whether this handle has been retired by a deregistration.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
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

    /// The bound account's phase and whether it is serving, for `/health`.
    ///
    /// Deliberately cheaper than [`AppState::account_views`]: no store read
    /// lock (the message count is detail, and `/health` is polled), and no
    /// discovery results (see [`AccountPhase`]).
    pub fn account_phase(&self) -> (AccountPhase, bool) {
        let status = bound_account(&self.accounts.lock()).map(|h| h.status());
        match status {
            Some(s) => (AccountPhase::from(s), s.is_ready()),
            None => (AccountPhase::Unregistered, false),
        }
    }

    /// Per-account views for `GET /api/v1/accounts`: the bound account (if
    /// any), plus each discovered-but-unregistered one as `awaiting_key`.
    /// Sorted by wxid.
    ///
    /// Detail only — readiness is [`AppState::account_phase`]'s job. This list
    /// deliberately cannot answer it: a discovered entry is `awaiting_key`
    /// forever until someone registers it, so folding these rows into a
    /// readiness verdict would pin a scanned-but-unused account to "not ready"
    /// (see [`AppState::set_discovered`]).
    pub fn account_views(&self) -> Vec<AccountStateView> {
        let mut views: Vec<AccountStateView> = {
            let accounts = self.accounts.lock();
            accounts
                .values()
                .map(|h| AccountStateView {
                    wxid: h.info.wxid.clone(),
                    state: h.status(),
                    message_count: h.store.read().total_messages(),
                    error: h.error.lock().clone(),
                    db_storage: h.info.db_storage.to_string_lossy().into_owned(),
                })
                .collect()
        };
        let registered: std::collections::HashSet<String> =
            views.iter().map(|v| v.wxid.clone()).collect();
        for info in self.discovered.lock().iter() {
            if !registered.contains(&info.wxid) {
                views.push(AccountStateView {
                    wxid: info.wxid.clone(),
                    state: AccountStatus::AwaitingKey,
                    message_count: 0,
                    error: None,
                    db_storage: info.db_storage.to_string_lossy().into_owned(),
                });
            }
        }
        views.sort_by(|a, b| a.wxid.cmp(&b.wxid));
        views
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountStateView {
    pub wxid: String,
    pub state: AccountStatus,
    pub message_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Resolved live-database directory (`<account>/db_storage`). Same field
    /// name the registration endpoint echoes back.
    pub db_storage: String,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    use handlers::*;
    Router::new()
        .route("/health", axum::routing::get(health::handler).post(health::handler))
        .route("/api/v1/health", axum::routing::get(health::handler).post(health::handler))
        .route(
            "/api/v1/accounts",
            axum::routing::get(accounts::list_handler).post(accounts::handler),
        )
        .route(
            "/api/v1/accounts/{wxid}",
            axum::routing::delete(accounts::delete_handler),
        )
        // Alias for clients and proxies that cannot issue DELETE.
        .route(
            "/api/v1/accounts/{wxid}/deregister",
            axum::routing::post(accounts::delete_handler),
        )
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
/// Resolve the request's paths and keys, then claim the binding and start the
/// build. Returns [`register_account`]'s verdict verbatim — in a `Bound` the
/// async build is already spawned, so the HTTP layer reports it as `accepted`.
pub async fn start_account(
    state: Arc<AppState>,
    body: crate::server::handlers::accounts::AccountBody,
) -> Result<BindOutcome, ApiError> {
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

    // A ready/indexing account re-registered (`Existing`) is reused with no
    // rebuild; `Occupied` means we lost the race against another registration
    // between the handler's fast-path check and here, incumbent untouched.
    // Both pass straight through — only `Bound` has a build to start.
    let handle = match register_account(&state, info.clone(), key_map, media_keys) {
        BindOutcome::Bound(h) => h,
        other => return Ok(other),
    };

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
        // A deregistration that landed while the build was running retired
        // this handle. It is already out of the registry and its store has
        // been cleared, so publishing a status, a watermark baseline or a
        // watcher now would resurrect an account nobody can reach.
        if handle2.is_stopped() {
            // INFO, not DEBUG: a minutes-long build was deliberately thrown
            // away. At the default `info` level this is the only trace that
            // deregistering an `indexing` account left work in flight, so it
            // has to be visible — and it names WHICH outcome was discarded,
            // because the guard sits ahead of the `match` and therefore
            // covers all three (built / failed / panicked, then deregistered).
            let outcome = match &result {
                Ok(Ok(n)) => format!("索引已完成（{n} 个数据库）"),
                Ok(Err(e)) => format!("索引失败: {e:#}"),
                Err(e) => format!("索引任务异常: {e}"),
            };
            tracing::info!(
                "[init] 账号 {} 在索引期间已注销，丢弃本次构建结果（{outcome}）",
                handle2.info.wxid
            );
            return;
        }
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
                // Deregistration between the guard above and here found the
                // watcher slot still empty, so it could not abort this task.
                // Abort the handle we are holding rather than re-reading the
                // slot, which would race with that same `take()`.
                if handle2.is_stopped() {
                    // DEBUG is right here: unlike the guard above, this is a
                    // nanosecond-wide race (deregistration landed between that
                    // guard and this line), not a discarded build.
                    tracing::debug!(
                        "[init] 账号 {} 注销与 watcher 启动竞态，已中止监视任务",
                        handle2.info.wxid
                    );
                    h.abort();
                } else {
                    *handle2.watcher.lock() = Some(h);
                }
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

    Ok(BindOutcome::Bound(handle))
}

/// Claim the single account binding and register the handle.
///
/// Two rules, in this order:
/// - **Occupied**: a DIFFERENT wxid holds the binding — rejected, and the
///   incumbent is left completely untouched (see [`bound_account`]). An
///   account in `error` still holds it: freeing it on failure would let one
///   transient decrypt error hand the server to another account without
///   anyone asking. Switching accounts takes an explicit deregistration.
/// - **Existing**: the SAME wxid is already `ready`/`indexing` — idempotent,
///   the live handle comes back with no rebuild and no watcher aborted. Only
///   an `error` (or awaiting-key) handle is replaced, so a corrected
///   registration recovers cleanly.
///
/// The guard and the insert share ONE `accounts` lock: taking it twice would
/// let two concurrent registrations for different wxids both see a free
/// binding and both insert.
pub fn register_account(
    state: &Arc<AppState>,
    info: AccountInfo,
    keys: crate::keystore::KeyMap,
    media_keys: Option<crate::keystore::ImageKeys>,
) -> BindOutcome {
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
    // No second owner of `sync` yet, so this lock cannot contend.
    let stopped = sync.lock().stop_flag();
    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(1), // indexing
        error: Mutex::new(None),
        store,
        sync,
        media_keys,
        watcher: Mutex::new(None),
        stopped,
    });

    let mut accounts = state.accounts.lock();
    if let Some(incumbent) = bound_account(&accounts) {
        if incumbent.info.wxid != handle.info.wxid {
            return BindOutcome::Occupied {
                wxid: incumbent.info.wxid.clone(),
                status: incumbent.status(),
            };
        }
        if matches!(incumbent.status(), AccountStatus::Ready | AccountStatus::Indexing) {
            return BindOutcome::Existing(incumbent);
        }
        // Same wxid in awaiting_key/error: replace it below and retire the old
        // handle so a build still in flight cannot write into the new store.
    }
    if let Some(old) = accounts.insert(handle.info.wxid.clone(), handle.clone()) {
        old.stopped.store(true, Ordering::SeqCst);
        if let Some(task) = old.watcher.lock().take() {
            task.abort();
        }
    }
    BindOutcome::Bound(handle)
}

/// Current watermarks across every ready account.
///
/// Collected registry-wide because the bus — and therefore the `sync` frame —
/// is process-wide, matching what the post-index baseline in [`start_account`]
/// publishes. Under the single-binding rule this is either one account's
/// watermarks or, after a deregistration, none.
pub fn current_watermarks(state: &AppState) -> Vec<(String, crate::store::Watermark)> {
    let handles: Vec<_> = {
        let accounts = state.accounts.lock();
        accounts
            .values()
            .filter(|h| h.status().is_ready())
            .cloned()
            .collect()
    };
    handles
        .iter()
        .flat_map(|h| {
            let guard = h.store.read();
            guard.watermarks.clone().into_iter().collect::<Vec<_>>()
        })
        .collect()
}

/// Result of a deregistration attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeregisterOutcome {
    /// The account was bound and is now gone.
    Deregistered {
        /// Its state-machine value immediately before the removal.
        previous: AccountStatus,
        /// Whether an index actually existed — false when the account never
        /// got past `indexing`.
        index_cleared: bool,
        /// Directory count removed under the export root (0 without a purge).
        purged_dirs: usize,
    },
    /// Nothing is bound; there is nothing to deregister.
    NotRegistered,
    /// A DIFFERENT account is bound. Deliberately not treated as success: the
    /// wxid in the path is a safety interlock, so a client that has drifted
    /// out of sync with the server learns that instead of believing it just
    /// removed something.
    WxidMismatch { occupied_by: String, status: AccountStatus },
}

/// Exported-media subdirectories the server itself creates, per
/// `<media_export_dir>/<talker>/<kind>/<file>` — see `media::export` and the
/// allow-list in `handlers::media`.
const EXPORT_KINDS: [&str; 4] = ["images", "voices", "videos", "emojis"];

/// Remove the exported-media directories this account produced, and nothing
/// else. Returns how many were removed.
///
/// Scoped deliberately narrowly: only `<root>/<talker>/<kind>` for a talker
/// this account actually had, and only for the four kinds the exporter writes.
/// `root` comes from `--media-export-dir` and may well be a directory the
/// operator also keeps other things in, so a recursive delete of the root is
/// never an option; the talker directory itself goes through `remove_dir`,
/// which refuses to touch it unless it is empty.
///
/// Note the export layout has no account dimension: two accounts that both
/// talked to the same talker share one directory, so a purge can remove files
/// the other account exported. That is why `purge_media` defaults to false.
fn purge_exported_media(root: &std::path::Path, talkers: &[String]) -> usize {
    let mut removed = 0usize;
    for talker in talkers {
        // Talkers come from the database, not the request, but they end up as
        // a path segment — same containment rule as the media route.
        let bad = talker.is_empty()
            || talker == "."
            || talker == ".."
            || talker.contains('/')
            || talker.contains('\\');
        if bad {
            tracing::warn!("[deregister] 跳过异常 talker 目录名: {talker:?}");
            continue;
        }
        let dir = root.join(talker);
        for kind in EXPORT_KINDS {
            let sub = dir.join(kind);
            match std::fs::remove_dir_all(&sub) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("[deregister] 清理导出媒体失败 {}: {e}", sub.display()),
            }
        }
        // Empty-only: anything the server did not put there survives.
        let _ = std::fs::remove_dir(&dir);
    }
    removed
}

/// Undo one account's registration: stop its sync, drop its index, and return
/// the server to the unregistered state it boots in.
///
/// Blocking (store lock, plus file IO when `purge_media` is set) — callers run
/// it on the blocking pool.
///
/// The step order is load-bearing:
/// 1. remove the handle from the registry FIRST, so no new request can resolve
///    it while the rest of the teardown runs;
/// 2. set `stopped` — lock-free by design (`sync`'s mutex can be held by a
///    `full_sync` for minutes) — so an in-flight poll discards its rows
///    instead of writing them into the store we are about to clear, and a
///    running build abandons its result instead of flipping to `ready`;
/// 3. abort the watcher, which only stops FUTURE passes: a pass already inside
///    `spawn_blocking` runs to completion regardless, which is what (2) covers;
/// 4. re-broadcast the watermark baseline for whatever remains ready, so a
///    client learns its watermarks are gone.
///
/// Two things are deliberately NOT done. The SSE replay history is left alone:
/// it is process-wide and its ids are a bus-level sequence, so clearing it
/// would break Last-Event-ID replay for subscribers that have nothing to do
/// with this account. And `discovered` is left alone, which is exactly the
/// right behavior for free: an account the startup scan found reappears as
/// `awaiting_key` (it really is still on this machine), while a client-only
/// account vanishes entirely (nothing here knows about it any more).
pub fn deregister_account(state: &AppState, wxid: &str, purge_media: bool) -> DeregisterOutcome {
    // 1. Claim the removal under one lock.
    let handle = {
        let mut accounts = state.accounts.lock();
        match bound_account(&accounts) {
            None => return DeregisterOutcome::NotRegistered,
            Some(b) if b.info.wxid != wxid => {
                return DeregisterOutcome::WxidMismatch {
                    occupied_by: b.info.wxid.clone(),
                    status: b.status(),
                }
            }
            Some(b) => {
                accounts.remove(wxid);
                b
            }
        }
    };
    let previous = handle.status();

    // 2. Retire the sync side without touching `handle.sync`'s mutex.
    handle.stopped.store(true, Ordering::SeqCst);

    // 3. Stop future watch passes.
    if let Some(task) = handle.watcher.lock().take() {
        task.abort();
    }

    // 4. Drop the index, collecting the talkers to purge while we still can.
    // The handle is already unreachable, so clearing its store is enough —
    // nothing else can observe it, and it dies with the last Arc.
    let (talkers, index_cleared) = {
        let mut guard = handle.store.write();
        let talkers: Vec<String> =
            if purge_media { guard.convs.keys().cloned().collect() } else { Vec::new() };
        let had_index = !guard.is_empty();
        *guard = Store::default();
        (talkers, had_index)
    };
    let _ = state.events.send(Event::Sync(current_watermarks(state)));

    let purged_dirs = if purge_media {
        purge_exported_media(&state.cfg.media_export_dir, &talkers)
    } else {
        0
    };
    tracing::info!(
        "[deregister] 账号 {wxid} 已注销 (原状态 {previous:?}, 索引已清理 {index_cleared}, 清理媒体目录 {purged_dirs})"
    );
    DeregisterOutcome::Deregistered { previous, index_cleared, purged_dirs }
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

    /// Claim the binding, asserting it was free.
    fn bind(state: &Arc<AppState>, wxid: &str) -> Arc<AccountHandle> {
        match register_account(state, info(wxid), crate::keystore::KeyMap::default(), None) {
            BindOutcome::Bound(h) => h,
            BindOutcome::Existing(_) => panic!("{wxid} was already bound"),
            BindOutcome::Occupied { wxid: other, .. } => panic!("binding held by {other}"),
        }
    }

    /// Discovered-but-unregistered accounts are reported, never gate readiness.
    ///
    /// Guards the (ii) choice: folding them into the readiness verdict would
    /// pin `/health` at `starting` forever when a client never registers one.
    #[test]
    fn discovered_accounts_are_reported_but_never_gate_readiness() {
        let state = test_state();
        state.set_discovered(vec![info("wxid_scanned")]);

        // Nothing bound yet: listed as awaiting_key, and NOT ready.
        let views = state.account_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].wxid, "wxid_scanned");
        assert_eq!(views[0].state, AccountStatus::AwaitingKey);
        assert_eq!(views[0].message_count, 0);
        assert_eq!(state.account_phase(), (AccountPhase::Unregistered, false));

        // Bind a DIFFERENT account and mark it ready. The still-unregistered
        // scanned account must not hold readiness back.
        bind(&state, "wxid_registered").set_status(AccountStatus::Ready);

        let views = state.account_views();
        assert!(
            state.account_phase().1,
            "an unregistered scanned account must not block readiness"
        );
        assert_eq!(views.len(), 2, "both are reported");
        // Sorted by wxid: registered < scanned.
        assert_eq!(views[0].wxid, "wxid_registered");
        assert_eq!(views[0].state, AccountStatus::Ready);
        assert_eq!(views[1].wxid, "wxid_scanned");
        assert_eq!(views[1].state, AccountStatus::AwaitingKey);
    }

    /// A bound account that is still indexing DOES gate readiness.
    #[test]
    fn registered_not_ready_account_gates_readiness() {
        let state = test_state();
        let a = bind(&state, "wxid_a");
        a.set_status(AccountStatus::Indexing);
        assert_eq!(state.account_phase(), (AccountPhase::Indexing, false));
        a.set_status(AccountStatus::Ready);
        assert_eq!(state.account_phase(), (AccountPhase::Ready, true));
    }

    /// Registering a scanned account replaces its discovery entry rather than
    /// listing the same wxid twice.
    #[test]
    fn registering_a_discovered_account_does_not_duplicate_it() {
        let state = test_state();
        state.set_discovered(vec![info("wxid_dup")]);
        let h = bind(&state, "wxid_dup");
        h.set_status(AccountStatus::Ready);
        let views = state.account_views();
        assert_eq!(views.len(), 1, "wxid must not appear twice");
        assert_eq!(views[0].state, AccountStatus::Ready);
        assert!(state.account_phase().1);
    }

    // --- single-account binding -------------------------------------------

    /// Only one account may hold the binding; a second wxid is rejected and the
    /// incumbent is left completely alone.
    #[test]
    fn a_second_wxid_is_rejected_and_the_incumbent_is_untouched() {
        let state = test_state();
        let a = bind(&state, "wxid_a");
        a.set_status(AccountStatus::Ready);

        match register_account(&state, info("wxid_b"), crate::keystore::KeyMap::default(), None) {
            BindOutcome::Occupied { wxid, status } => {
                assert_eq!(wxid, "wxid_a");
                assert_eq!(status, AccountStatus::Ready);
            }
            _ => panic!("a second wxid must not bind"),
        }
        // The incumbent kept its state, its handle and its (empty) watcher slot.
        assert_eq!(a.status(), AccountStatus::Ready);
        assert!(!a.is_stopped(), "the rejected registration must not retire it");
        let accounts = state.accounts.lock();
        assert_eq!(accounts.len(), 1, "the rejected wxid must not be inserted");
        assert!(accounts.contains_key("wxid_a"));
    }

    /// An `error` account still HOLDS the binding: one transient decrypt
    /// failure must not hand the server to a different account.
    #[test]
    fn an_error_account_still_holds_the_binding() {
        let state = test_state();
        let a = bind(&state, "wxid_a");
        a.set_status(AccountStatus::Error);

        assert!(
            matches!(
                register_account(&state, info("wxid_b"), crate::keystore::KeyMap::default(), None),
                BindOutcome::Occupied { .. }
            ),
            "error does not free the binding"
        );
        // ...but re-registering the SAME wxid retries the build, and retires the
        // old handle so its in-flight build cannot write into the new store.
        match register_account(&state, info("wxid_a"), crate::keystore::KeyMap::default(), None) {
            BindOutcome::Bound(h) => assert_eq!(h.status(), AccountStatus::Indexing),
            _ => panic!("the same wxid in error must be replaced"),
        }
        assert!(a.is_stopped(), "the replaced handle must be retired");
    }

    /// Re-registering a live account is idempotent: same handle, no rebuild.
    #[test]
    fn re_registering_the_same_wxid_is_idempotent() {
        let state = test_state();
        let a = bind(&state, "wxid_a");
        for st in [AccountStatus::Indexing, AccountStatus::Ready] {
            a.set_status(st);
            match register_account(&state, info("wxid_a"), crate::keystore::KeyMap::default(), None)
            {
                BindOutcome::Existing(h) => {
                    assert!(Arc::ptr_eq(&h, &a), "must return the LIVE handle");
                    assert!(!a.is_stopped(), "an idempotent hit must not retire it");
                }
                _ => panic!("{st:?} must be idempotent"),
            }
        }
    }

    /// After a deregistration the binding is free for a different account.
    #[test]
    fn deregistration_frees_the_binding_for_another_wxid() {
        let state = test_state();
        bind(&state, "wxid_a").set_status(AccountStatus::Ready);
        assert!(matches!(
            deregister_account(&state, "wxid_a", false),
            DeregisterOutcome::Deregistered { previous: AccountStatus::Ready, .. }
        ));
        assert!(matches!(
            register_account(&state, info("wxid_b"), crate::keystore::KeyMap::default(), None),
            BindOutcome::Bound(_)
        ));
    }

    // --- /health phase -----------------------------------------------------

    #[test]
    fn phase_mapping_never_leaks_discovery() {
        assert_eq!(AccountPhase::from(AccountStatus::AwaitingKey), AccountPhase::Unregistered);
        assert_eq!(AccountPhase::from(AccountStatus::Indexing), AccountPhase::Indexing);
        assert_eq!(AccountPhase::from(AccountStatus::Ready), AccountPhase::Ready);
        assert_eq!(AccountPhase::from(AccountStatus::Error), AccountPhase::Error);

        // A scanned-but-unregistered account is NOT a binding: `/health` must
        // read `unregistered` even though the detail endpoint lists it.
        let state = test_state();
        state.set_discovered(vec![info("wxid_scanned")]);
        assert_eq!(state.account_phase(), (AccountPhase::Unregistered, false));
        assert_eq!(state.account_views().len(), 1, "the detail view still lists it");
    }

    // --- deregistration ----------------------------------------------------

    /// The happy path: the binding is gone, the index is cleared, the handle is
    /// retired, and `/health` reads `unregistered` again.
    #[test]
    fn deregistering_clears_the_index_and_retires_the_handle() {
        let state = test_state();
        let h = bind(&state, "wxid_a");
        h.set_status(AccountStatus::Ready);
        {
            let mut store = h.store.write();
            store.convs.insert("talker_a".into(), Vec::new());
            store.watermarks.insert("message_0.db:Msg".into(), crate::store::Watermark::default());
        }

        let outcome = deregister_account(&state, "wxid_a", false);
        assert_eq!(
            outcome,
            DeregisterOutcome::Deregistered {
                previous: AccountStatus::Ready,
                index_cleared: true,
                purged_dirs: 0,
            }
        );
        assert!(h.is_stopped(), "the sync must be retired");
        assert!(h.store.read().watermarks.is_empty(), "the index must be dropped");
        assert!(state.accounts.lock().is_empty());
        assert_eq!(state.account_phase(), (AccountPhase::Unregistered, false));
    }

    /// Deregistering mid-`indexing` is allowed — a stuck build is exactly what
    /// a client needs to be able to clear — and reports no index to clear.
    #[test]
    fn deregistering_mid_indexing_is_allowed() {
        let state = test_state();
        let h = bind(&state, "wxid_a"); // indexing
        assert_eq!(
            deregister_account(&state, "wxid_a", false),
            DeregisterOutcome::Deregistered {
                previous: AccountStatus::Indexing,
                index_cleared: false,
                purged_dirs: 0,
            }
        );
        // The retirement flag is what stops the in-flight build from flipping
        // this handle to `ready` after the fact.
        assert!(h.is_stopped());
        assert!(h.sync.lock().is_stopped());
    }

    /// The `wxid` in the path is an interlock: a mismatch changes nothing.
    #[test]
    fn deregistering_the_wrong_wxid_touches_nothing() {
        let state = test_state();
        let h = bind(&state, "wxid_a");
        h.set_status(AccountStatus::Ready);
        assert_eq!(
            deregister_account(&state, "wxid_b", true),
            DeregisterOutcome::WxidMismatch {
                occupied_by: "wxid_a".into(),
                status: AccountStatus::Ready,
            }
        );
        assert!(!h.is_stopped());
        assert_eq!(h.status(), AccountStatus::Ready);
        assert_eq!(state.accounts.lock().len(), 1);
    }

    /// Idempotent: retrying a completed deregistration is not an error.
    #[test]
    fn deregistering_nothing_is_idempotent() {
        let state = test_state();
        assert_eq!(deregister_account(&state, "wxid_a", false), DeregisterOutcome::NotRegistered);
        bind(&state, "wxid_a");
        assert!(matches!(
            deregister_account(&state, "wxid_a", false),
            DeregisterOutcome::Deregistered { .. }
        ));
        assert_eq!(deregister_account(&state, "wxid_a", false), DeregisterOutcome::NotRegistered);
    }

    /// A scanned account reverts to `awaiting_key` keeping its path; a
    /// client-introduced one disappears entirely. Both fall out of leaving
    /// `discovered` alone — the two lists are independent.
    #[test]
    fn deregistration_reverts_scanned_accounts_and_drops_client_only_ones() {
        let state = test_state();
        state.set_discovered(vec![info("wxid_scanned")]);
        bind(&state, "wxid_scanned").set_status(AccountStatus::Ready);
        deregister_account(&state, "wxid_scanned", false);
        let views = state.account_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, AccountStatus::AwaitingKey);
        assert!(views[0].db_storage.contains("wxid_scanned"), "keeps its path");
        assert!(!state.account_phase().1, "a scanned entry is not a binding");

        bind(&state, "wxid_client_only").set_status(AccountStatus::Ready);
        deregister_account(&state, "wxid_client_only", false);
        let views = state.account_views();
        assert_eq!(views.len(), 1, "the client-only account is gone entirely");
        assert_eq!(views[0].wxid, "wxid_scanned");
    }

    /// The purge stays inside the layout the exporter writes and never
    /// recursive-deletes the export root.
    #[test]
    fn purge_exported_media_stays_inside_the_known_layout() {
        let root = std::path::PathBuf::from("target/test-tmp/purge-scope");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("talker_a/images")).unwrap();
        std::fs::create_dir_all(root.join("talker_a/voices")).unwrap();
        std::fs::write(root.join("talker_a/images/1.jpg"), b"x").unwrap();
        // Not ours: neither an EXPORT_KINDS directory nor a talker we pass.
        std::fs::create_dir_all(root.join("talker_a/operator_notes")).unwrap();
        std::fs::create_dir_all(root.join("talker_b/images")).unwrap();
        std::fs::write(root.join("keep-me.txt"), b"x").unwrap();

        // `../escape` is skipped by the containment check, not resolved.
        let removed = purge_exported_media(&root, &["talker_a".into(), "../escape".into()]);
        assert_eq!(removed, 2, "images + voices");
        assert!(!root.join("talker_a/images").exists());
        assert!(!root.join("talker_a/voices").exists());
        assert!(
            root.join("talker_a/operator_notes").exists(),
            "a non-export subdirectory survives, and so does its talker dir"
        );
        assert!(root.join("talker_b/images").exists(), "another talker is untouched");
        assert!(root.join("keep-me.txt").exists(), "the export root is never wiped");

        // A talker whose directory holds nothing but exports is removed whole.
        assert_eq!(purge_exported_media(&root, &["talker_b".into()]), 1);
        assert!(!root.join("talker_b").exists());
        let _ = std::fs::remove_dir_all(&root);
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
