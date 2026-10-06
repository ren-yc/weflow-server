//! Handwritten behavior layer on top of [`crate::generated`].
//!
//! The generated face covers shapes: request URLs, response types, error
//! decoding. It cannot cover behavior, because the description documents
//! neither pagination loops nor reconnection - and its query parameters are
//! not declared at all (the server's description currently lists only
//! responses), so every request here assembles its own query string and
//! reuses the generated types purely for deserialization.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use futures_util::StreamExt;

use crate::generated::r#gen::types as gen_types;

/// Everything the behavior layer can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered, but not with a usable body.
    #[error("HTTP {status} on {url}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Request URL, for the operator reading the log.
        url: String,
    },
    /// Transport-level failure (connect, timeout, reset).
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    /// The body was not the JSON shape the generated types declare.
    #[error("response did not match the documented shape: {0}")]
    Shape(#[from] serde_json::Error),
    /// `ensure_ready` hit its deadline before the account reached `ready`.
    #[error("account did not become ready within {timeout:?} (last state: {last_state})")]
    NotReady {
        /// The timeout that was requested.
        timeout: Duration,
        /// Final observed phase, so the caller can tell polling from stuck.
        last_state: String,
    },
    /// A time bound was neither `YYYYMMDD` nor unix seconds.
    #[error("invalid time bound {field}={value:?}: expected YYYYMMDD or unix seconds")]
    BadDate {
        /// Which parameter.
        field: &'static str,
        /// What was passed.
        value: String,
    },
    /// The body decoded as JSON but did not carry what this call requires.
    #[error("{url}: {detail}")]
    UnexpectedBody {
        /// Request URL, for the operator reading the log.
        url: String,
        /// What was expected, and what arrived instead.
        detail: String,
    },
    /// The server answered 200 with a **business refusal** state.
    ///
    /// Waiting for `ready` after a refusal can only time out and hide the
    /// cause, so the refusal surfaces here instead: the state name is the
    /// actionable part (`account_conflict` names who holds the binding; the
    /// extra fields are on the body of [`Client::register`]).
    #[error("registration refused: state={state} ({url})")]
    Refused {
        /// The refusal state the server named.
        state: String,
        /// Request URL.
        url: String,
    },
}

/// 200-with-refusal vocabulary, shared verbatim with the Python SDK. The
/// weflow server currently emits only `account_conflict`; the qqflow sibling
/// adds the rest, and one shared set keeps the two SDKs symmetric against
/// future server-side additions.
const REFUSAL_STATES: [&str; 4] = [
    "account_conflict",
    "invalid_key",
    "invalid_db_path",
    "unknown_qq",
];

/// Result alias for the behavior layer.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Raw `POST /api/v1/accounts` outcome.
///
/// HTTP 200 covers several business states with **different shapes**
/// (`accepted` / `in_progress` / `already_ready` carry a status; a conflict
/// carries who holds the binding; a mismatch carries neither). Only the two
/// fields every state has are typed here; the decoded body stays available so
/// a refusal keeps its extra fields.
///
/// A refusal is a **value**, not an error: what to do about it (retry, give
/// up, tell the operator which account holds the binding) is the caller's
/// decision — the caller is the one holding the configuration to compare
/// against.
#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    /// Business state string (`accepted` / `in_progress` / `already_ready` /
    /// `account_conflict` / `wxid_mismatch` / …).
    pub state: String,
    /// Account status, when the state carries one (`ready` / `indexing` /
    /// `error`).
    pub status: Option<String>,
    /// The whole decoded body.
    pub body: serde_json::Value,
}

impl RegisterOutcome {
    fn from_body(url: &str, body: serde_json::Value) -> Result<Self> {
        let Some(state) = body.get("state").and_then(|v| v.as_str()) else {
            return Err(ClientError::UnexpectedBody {
                url: url.to_string(),
                detail: format!("no string `state` in the body: {body}"),
            });
        };
        // Read the status as a string rather than through the generated enum:
        // the enum only covers the states this build knows, and an unknown one
        // must not fail the whole call.
        let status = body.get("status").and_then(|v| v.as_str()).map(str::to_string);
        Ok(Self { state: state.to_string(), status, body })
    }
}

/// Query for the native messages face ([`Client::list_messages`]).
///
/// Time bounds accept either unix seconds or `YYYYMMDD`; as an upper bound a
/// bare date covers its **whole day**. Bounds are validated here so a typo
/// fails fast with [`ClientError::BadDate`] instead of a 400.
#[derive(Debug, Clone, Default)]
pub struct MessageQuery {
    /// Conversation key (`…@chatroom` for groups).
    pub talker: String,
    /// Substring filter over the message body.
    pub keyword: Option<String>,
    /// Inclusive lower bound.
    pub start: Option<String>,
    /// Inclusive upper bound.
    pub end: Option<String>,
    /// Page size (server default 100, maximum 10000).
    pub limit: Option<u32>,
    /// Offset cursor: advance it by the page size until `has_more` is false.
    pub offset: Option<u64>,
    /// Export this page's media before answering (`media=1`); the files land
    /// under the envelope's `media.exportPath`.
    pub media: bool,
}

impl MessageQuery {
    /// A query for one conversation, server defaults everywhere else.
    pub fn new(talker: impl Into<String>) -> Self {
        Self { talker: talker.into(), ..Self::default() }
    }

    fn params(&self, endpoint: &str) -> Result<BTreeMap<&'static str, String>> {
        if self.talker.is_empty() {
            return Err(ClientError::UnexpectedBody {
                // The endpoint this query is about to hit. One query struct serves
                // both faces, so a hardcoded URL pointed a `chatlab_messages`
                // failure at the other endpoint.
                url: endpoint.to_string(),
                detail: "talker must not be empty".to_string(),
            });
        }
        let mut p = BTreeMap::new();
        p.insert("talker", self.talker.clone());
        if let Some(k) = &self.keyword {
            p.insert("keyword", k.clone());
        }
        for (field, value) in [("start", &self.start), ("end", &self.end)] {
            if let Some(v) = value {
                validate_time_bound(field, v)?;
                p.insert(field, v.clone());
            }
        }
        if let Some(l) = self.limit {
            p.insert("limit", l.to_string());
        }
        if let Some(o) = self.offset {
            p.insert("offset", o.to_string());
        }
        if self.media {
            p.insert("media", "1".to_string());
        }
        Ok(p)
    }
}

/// Query for [`Client::contacts`].
#[derive(Debug, Clone, Default)]
pub struct ContactsQuery {
    /// Page size (server default 100, maximum 10000).
    pub limit: Option<u32>,
    /// Offset cursor: advance it by the page size until `has_more` is false.
    pub offset: Option<u64>,
    /// Substring filter over display name / remark / nickname.
    pub keyword: Option<String>,
}

/// A time bound is either a bare `YYYYMMDD` date or unix seconds — the server
/// parses both, so this check accepts both and rejects everything else.
/// Percent-encode one path segment (unreserved set plus `@`, which real wxids use).
///
/// Why: `format!("/api/v1/sessions/{talker}/messages")` sends a `#` or `?` inside
/// an id straight through — the server reads the rest as a fragment or query and
/// answers 404 — while the Python client's httpx percent-encodes it. The same
/// input must not mean two things in two languages.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'@') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn validate_time_bound(field: &'static str, value: &str) -> Result<()> {
    let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    if digits {
        Ok(())
    } else {
        Err(ClientError::BadDate { field, value: value.to_string() })
    }
}

/// One live server event, decoded from the SSE stream.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// A new message arrived.
    New(gen_types::EventNew),
    /// A message was revoked.
    Revoke(gen_types::EventRevoke),
    /// The server rebroadcast its watermarks (carries `generation`).
    Sync(gen_types::EventSync),
    /// The notification face's meta-only frame (or an unknown kind, passed
    /// through as meta so a new server event kind degrades instead of killing
    /// the stream).
    Notification(gen_types::NotificationFrame),
}

impl ServerEvent {
    /// The event kind name this variant was decoded from.
    pub fn kind(&self) -> &str {
        match self {
            ServerEvent::New(e) => e.event.as_str(),
            ServerEvent::Revoke(e) => e.event.as_str(),
            ServerEvent::Sync(e) => e.event.as_str(),
            ServerEvent::Notification(e) => e.event.as_str(),
        }
    }

    /// Generation carried by `sync` frames (0 otherwise). A jump means the
    /// replay buffer cannot fill the gap - fall back to [`Client::drain_session`].
    pub fn generation(&self) -> u64 {
        match self {
            ServerEvent::Sync(s) => s.generation.max(0) as u64,
            _ => 0,
        }
    }
}

/// Connect budget for every request the SDK makes. Published so a caller can
/// compare against it instead of guessing.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read budget for ordinary JSON requests, charged **per read operation** and
/// reset by each successful chunk (deliberately not a total deadline): a large but
/// steadily streaming answer is not a stall. Same semantics as httpx's read
/// timeout, so this constant means one thing in both SDKs.
/// Requests unbounded by construction (a full-roster message count, a media body,
/// the SSE stream) deliberately do not use it.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Client for one weflow-server instance. Cloneable; shares the connection
/// pools.
#[derive(Clone)]
pub struct Client {
    /// Ordinary JSON requests: [`CONNECT_TIMEOUT`] plus [`READ_TIMEOUT`].
    http: reqwest::Client,
    /// No read bound: a whole-roster message count, a media body of arbitrary
    /// size, a sync pass, and the SSE stream. Connect stays bounded (a server
    /// that is not there must fail fast) but a slow answer is not an error.
    /// The stream needs this too: a read bound is charged per read operation
    /// while the server pings every 25s, so the JSON budget leaves only a 5s
    /// margin that one proxy stall burns through on a healthy idle stream.
    http_long: reqwest::Client,
    base_url: String,
    token: String,
}

impl Client {
    /// Point a client at a server. `token` is the API token the server
    /// printed on first start (or `--show-token`).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let json = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // read_timeout, not timeout: the latter is a *total* deadline from "start
            // connecting" to "response body finished", which would cut off a large
            // answer that is streaming fine. httpx (and this crate's own stream loop)
            // charge a read bound per read operation, resetting on each successful
            // chunk - the two SDKs must mean the same thing by "30s read budget".
            .read_timeout(READ_TIMEOUT)
            .build()
            // reqwest only fails here when a TLS backend cannot initialize.
            // Falling back to an unbudgeted client would silently drop the
            // published default rather than tell anyone about it.
            .unwrap_or_else(|_| reqwest::Client::new());
        let long = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http: json,
            http_long: long,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    /// The base URL requests are built against.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str, query: &BTreeMap<&str, String>) -> Result<T> {
        self.get_json_on(&self.http, path, query).await
    }

    /// Same request on an explicitly chosen client: the two budgets are the
    /// whole difference between a JSON page and a full-roster count.
    async fn get_json_on<T: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        path: &str,
        query: &BTreeMap<&str, String>,
    ) -> Result<T> {
        let url = self.url(path);
        let resp = http
            .get(&url)
            .bearer_auth(&self.token)
            .query(query)
            .send()
            .await?;
        Self::decode(resp, &url).await
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response, url: &str) -> Result<T> {
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Status { status: status.as_u16(), url: url.to_string() });
        }
        // 先拿字节、再单独解码：`resp.json::<T>()` 把**解码失败**也包成 reqwest::Error，
        // 于是"服务端给了不符合承诺形状的东西"会被记成 Transport（连接故障），而调用方正是按
        // ClientError 的变体分支处理的 —— 形状损坏与网络断了混成一类，两边都失去了区分力。
        let body = resp.bytes().await?;
        serde_json::from_slice(&body).map_err(ClientError::Shape)
    }

    // ---- ensure_ready ---------------------------------------------------

    /// Register (idempotently) and poll until the account is `ready`.
    ///
    /// A 503 from business endpoints is *waiting*, not an error: the server is
    /// building its index. Errors are reserved for the deadline being hit or
    /// the account landing in `error`.
    pub async fn ensure_ready(
        &self,
        wxid: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<()> {
        // Registration and waiting are two primitives; this is their
        // composition. One implementation per endpoint means a change to the
        // registration contract cannot land in half the SDK.
        let outcome = self.register(body).await?;
        if REFUSAL_STATES.contains(&outcome.state.as_str()) {
            return Err(ClientError::Refused {
                state: outcome.state,
                url: self.url("/api/v1/accounts"),
            });
        }
        self.wait_ready(wxid, timeout).await
    }

    // ---- health / accounts / register / wait_ready --------------------------

    /// `GET /health` — liveness and account phase. **Unauthenticated.**
    ///
    /// Deliberately carries no account identity: confirming *which* account is
    /// bound requires [`Client::accounts`]. Useful as a cheap "is it up, and
    /// what is it doing" probe before spending an authenticated call.
    pub async fn health(&self) -> Result<gen_types::Health> {
        let url = self.url("/health");
        let resp = self.http.get(&url).send().await?;
        Self::decode(resp, &url).await
    }

    /// `GET /api/v1/accounts` — one entry per bound account.
    ///
    /// The failure reason (`error`) and the message count live only here;
    /// `/health` collapses everything to a scalar phase.
    pub async fn accounts(&self) -> Result<Vec<gen_types::AccountStateView>> {
        let listing: gen_types::AccountsList =
            self.get_json("/api/v1/accounts", &BTreeMap::new()).await?;
        Ok(listing.accounts)
    }

    /// `POST /api/v1/accounts` — register, returning the raw outcome
    /// **without waiting**.
    ///
    /// The waiting half is [`Client::wait_ready`]. Callers that classify the
    /// answer themselves (a refusal is a business state, not an HTTP error)
    /// use this pair instead of [`Client::ensure_ready`].
    pub async fn register(&self, body: &serde_json::Value) -> Result<RegisterOutcome> {
        let url = self.url("/api/v1/accounts");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let value: serde_json::Value = Self::decode(resp, &url).await?;
        RegisterOutcome::from_body(&url, value)
    }

    /// `POST /api/v1/sync` — run one manual incremental sync and return its
    /// counters.
    ///
    /// The watcher runs the same sync on file changes, so this exists for a
    /// caller that must have a fresh read *now* rather than eventually (the
    /// `sync` CLI subcommand and the `sync_now` MCP tool both do). It is a
    /// write against the server's state — it advances watermarks and may export
    /// media — so it is deliberately absent from every polling path: only a
    /// caller that asked for it triggers it.
    pub async fn sync_now(&self) -> Result<gen_types::SyncResult> {
        let url = self.url("/api/v1/sync");
        // A sync pass indexes and may export media: bounded only by the library size.
        let resp = self.http_long.post(&url).bearer_auth(&self.token).send().await?;
        Self::decode(resp, &url).await
    }

    /// Poll until `wxid` reports `ready`. **Wait-only**: never registers.
    ///
    /// Intermediate states are waiting, not errors — only the deadline and the
    /// `error` state fail. Registration belongs to the caller because the
    /// caller holds the key material and the configuration that identity is
    /// compared against.
    pub async fn wait_ready(&self, wxid: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_state = "not-registered".to_string();
        loop {
            match self.accounts().await {
                Ok(accounts) => match accounts.iter().find(|a| a.wxid == wxid) {
                    Some(a) if a.state == gen_types::AccountStatus::Ready => return Ok(()),
                    Some(a) if a.state == gen_types::AccountStatus::Error => {
                        return Err(ClientError::NotReady {
                            timeout,
                            last_state: format!("error: {}", a.error.clone().unwrap_or_default()),
                        });
                    }
                    Some(a) => last_state = a.state.to_string(),
                    None => last_state = "not-registered".to_string(),
                },
                // A dropped round is **waiting**, not failure. The whole point of a
                // deadline-based poll is to ride out a transient connect blip or a
                // momentary 5xx while the index is still coming up - aborting the
                // wait on one such round would contradict this method's own promise
                // that only the deadline and the account's `error` state fail. A 4xx
                // (bad token, wrong route) is a real misconfiguration, and a
                // `Shape` means the listing stopped matching the contract: both
                // propagate rather than silently waiting.
                Err(ClientError::Transport(_)) => {}
                Err(ClientError::Status { status, .. }) if (500..=599).contains(&status) => {}
                Err(e) => return Err(e),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::NotReady { timeout, last_state });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    // ---- pull_page / drain_session ---------------------------------------

    /// Fetch **one page** of the ChatLab Pull cursor loop.
    ///
    /// [`Client::drain_session`] is this in a loop; the single-page entry
    /// exists for callers that must bound a single request — an MCP tool has
    /// a per-call budget, and "drain everything" is exactly what it must not
    /// do.
    ///
    /// `since` is **exclusive** and `offset` advances within one timestamp
    /// group. Both cursors come back in `sync` and must be echoed verbatim:
    /// the server pages by (timestamp group, offset), and a client-derived
    /// cursor is how pages get silently skipped or replayed.
    ///
    /// `limit` is a per-page cap (the server caps it at 5000). `None` means
    /// the server default, which is also 5000.
    pub async fn pull_page(
        &self,
        talker: &str,
        since: Option<i64>,
        offset: u64,
        limit: Option<u32>,
    ) -> Result<gen_types::PullEnvelope> {
        let mut q = BTreeMap::new();
        if let Some(s) = since {
            q.insert("since", s.to_string());
        }
        if offset != 0 {
            q.insert("offset", offset.to_string());
        }
        if let Some(l) = limit {
            q.insert("limit", l.to_string());
        }
        self.get_json(&format!("/api/v1/sessions/{}/messages", encode_path_segment(talker)), &q)
            .await
    }

    /// Drain one session through the ChatLab Pull cursor loop, calling
    /// `on_page` per page. Cursors are echoed verbatim: `next_since` /
    /// `next_offset` come back exactly as the server sent them, because the
    /// server pages by (timestamp group, offset) and a client-derived cursor
    /// is how pages get silently skipped or replayed.
    pub async fn drain_session<F>(
        &self,
        talker: &str,
        since: Option<i64>,
        mut on_page: F,
    ) -> Result<u64>
    where
        F: FnMut(&[gen_types::PullMessage]) -> Result<()>,
    {
        let mut next_since = since;
        let mut next_offset = 0u64;
        let mut total = 0u64;
        loop {
            let page = self.pull_page(talker, next_since, next_offset, None).await?;
            total += page.messages.len() as u64;
            on_page(&page.messages)?;
            if !page.sync.has_more {
                return Ok(total);
            }
            next_since = Some(page.sync.next_since);
            next_offset = page.sync.next_offset;
        }
    }

    // ---- list_all_sessions -------------------------------------------------

    /// Fetch the complete session list via offset paging.
    ///
    /// The list is a live view: pages may shift between requests. The guard
    /// is "usernames within a page are unique" (the server guarantees that),
    /// so duplicates *across* pages are recorded as a warning and collapsed -
    /// losing data silently is the failure mode this loop must not have.
    ///
    /// `page_size` is the server-side page size (the server caps it at
    /// 10000). Leave it `None` for the server default; a polling consumer
    /// that re-reads the list every cycle should ask for the maximum instead,
    /// because the request count is `sessions / page_size` and the server's
    /// default page is two orders of magnitude smaller than the cap.
    ///
    /// `keyword` filters **server-side** (case-insensitive), so pagination
    /// walks the filtered list rather than trimming after the fact — trimming
    /// after the fact would stop at the first short page and silently drop
    /// matches that live further out.
    pub async fn list_all_sessions(
        &self,
        page_size: Option<u32>,
        keyword: Option<&str>,
    ) -> Result<Vec<gen_types::SessionNative>> {
        let mut out: Vec<gen_types::SessionNative> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut offset = 0usize;
        loop {
            let mut q = BTreeMap::new();
            q.insert("offset", offset.to_string());
            if let Some(n) = page_size {
                q.insert("limit", n.to_string());
            }
            if let Some(k) = keyword {
                q.insert("keyword", k.to_string());
            }
            let page: gen_types::SessionsNative =
                self.get_json("/api/v1/sessions", &q).await?;
            let count = page.sessions.len();
            for s in page.sessions {
                if seen.insert(s.username.clone()) {
                    out.push(s);
                } else {
                    log::warn!("session list shifted during pagination: duplicate {} collapsed", s.username);
                }
            }
            if count == 0 {
                return Ok(out);
            }
            offset += count;
        }
    }

    // ---- media_bytes -------------------------------------------------------

    /// Fetch media bytes for a handle that names content uniquely
    /// (content-digest-derived names only; the server 404s anything else).
    ///
    /// One automatic retry after a 404: the caller may have serialized the
    /// handle before the export finished, and `media=1` re-export is what
    /// mints the file. If the retry also 404s, the handle was not
    /// exportable to begin with and the error is returned as-is.
    /// `talker` is the **session id** the export side door needs; it is NOT
    /// `message.account_name` (that is the sender's display name, which only
    /// coincides with the session id for 1:1 chats where the display name was
    /// never customized - real data breaks the coincidence, so the caller must
    /// own the value).
    pub async fn media_bytes(&self, message: &gen_types::ChatlabMessage, talker: &str) -> Result<bytes::Bytes> {
        let Some(m) = &message.media else {
            // 本地前置条件失败：还没发请求，也谈不上响应体——归 UnexpectedBody
            // 是权宜（它带着 url/detail 两个槽位）。url 必须是这条调用真正面向的
            // 端点而不是散文：调用方按 url 归因时，散文会让分类静默失配。
            return Err(ClientError::UnexpectedBody {
                url: self.url("/api/v1/media"),
                detail: "message carries no media handle".to_string(),
            });
        };
        // 与 Python 侧同规：空 talker 会问一个不同的问题（导出空会话），
        // 服务端答空结果，重试 404 把「参数无效」伪装成「句柄不可导出」。
        if talker.trim().is_empty() {
            return Err(ClientError::UnexpectedBody {
                url: self.url("/api/v1/media"),
                detail: "talker must not be empty".to_string(),
            });
        }
        let name = &m.file_name;
        if name.trim().is_empty() {
            // 与 talker／chatroom 同规的空值前置拒绝：空句柄打到另一个路径上，
            // 服务端答「查无此文件」，于是「你没给名字」被伪装成「句柄不可导出」。
            return Err(ClientError::UnexpectedBody {
                url: self.url("/api/v1/media"),
                detail: "media file_name must not be empty".to_string(),
            });
        }
        let url_path = format!("/api/v1/media/{}", encode_path_segment(name));
        let url = self.url(&url_path);
        let resp = self.http_long.get(&url).bearer_auth(&self.token).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Trigger export, then retry once.
            let mut q = BTreeMap::new();
            q.insert("talker", talker.to_string());
            q.insert("media", "1".to_string());
            let _: gen_types::ChatlabMessages = self
                .get_json("/chatlab/messages", &q)
                .await?;
            let retry = self.http_long.get(&url).bearer_auth(&self.token).send().await?;
            return Self::decode_bytes(retry, &url).await;
        }
        Self::decode_bytes(resp, &url).await
    }

    async fn decode_bytes(resp: reqwest::Response, url: &str) -> Result<bytes::Bytes> {
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Status { status: status.as_u16(), url: url.to_string() });
        }
        Ok(resp.bytes().await?)
    }

    // ---- list_messages / contacts / media_bytes_by_id -----------------------

    /// `GET /api/v1/messages` — the **native** messages face.
    ///
    /// Descending by time, offset-paged: advance `offset` by the page size
    /// until `has_more` is false. This is the face that carries what the
    /// ChatLab shape drops — `rawContent`, `isSend`, `localType` — and the
    /// only one that can export media (`media = true`).
    ///
    /// Prefer [`Client::drain_session`] when both faces would do: the Pull
    /// cursor is stable across a live database, while offset paging over a
    /// growing table can shift.
    pub async fn list_messages(&self, q: &MessageQuery) -> Result<gen_types::MessagesNative> {
        self.get_json("/api/v1/messages", &q.params("/api/v1/messages")?).await
    }

    /// `GET /chatlab/messages` — the **ChatLab-shaped** messages face.
    ///
    /// Same query surface as [`Client::list_messages`] (`talker` required;
    /// `keyword`/`start`/`end`/`limit`/`offset`/`media`), different envelope:
    /// ascending by time, ChatLab type codes, `media` on the message,
    /// `count`/`page` for paging, and **no** `success` key. Project from this
    /// face when the caller wants ChatLab field names; the native face is the
    /// only one carrying `rawContent`/`isSend`.
    ///
    /// `offset` is this face's paging cursor (the wire also accepts `cursor`;
    /// the two are the same integer, so advancing by the page size and
    /// passing `page.nextCursor` are equivalent).
    pub async fn chatlab_messages(
        &self,
        q: &MessageQuery,
    ) -> Result<gen_types::ChatlabMessages> {
        self.get_json("/chatlab/messages", &q.params("/chatlab/messages")?).await
    }

    /// `GET /api/v1/contacts` — one page of the contact list.
    ///
    /// Contact detail is not part of the ChatLab shape at all; this is the
    /// only source for display names, remarks and aliases.
    pub async fn contacts(&self, q: &ContactsQuery) -> Result<gen_types::Contacts> {
        let mut params = BTreeMap::new();
        if let Some(l) = q.limit {
            params.insert("limit", l.to_string());
        }
        if let Some(o) = q.offset {
            params.insert("offset", o.to_string());
        }
        if let Some(k) = &q.keyword {
            params.insert("keyword", k.clone());
        }
        self.get_json("/api/v1/contacts", &params).await
    }

    /// `GET /api/v1/group-members` — 群成员（**名册 ∪ 发言人**）。
    ///
    /// 潜水成员（名册里、从未发言）也会出现，`message_count` 为 0 —— 只列发言人
    /// 会让「群里有谁」的答案取决于谁最近说过话。`include_message_counts` 为
    /// `true` 时服务端才数真实计数（否则整列回 0：计数是全会话扫描，不是免费的）。
    /// 名册与消息都在内存索引里：本请求不读盘、也不触发同步。
    pub async fn group_members(
        &self,
        chatroom: &str,
        include_message_counts: bool,
    ) -> Result<gen_types::GroupMembers> {
        if chatroom.is_empty() {
            // 空群号问的是另一个问题：服务端会答成一个空名册，于是"这个群没有成员"与
            // "你没告诉我是哪个群"在调用方看来一模一样。与 talker 同规本地拒绝、不发请求。
            return Err(ClientError::UnexpectedBody {
                url: "/api/v1/group-members".to_string(),
                detail: "chatroom must not be empty".to_string(),
            });
        }
        let mut params = BTreeMap::new();
        params.insert("chatroomId", chatroom.to_string());
        if include_message_counts {
            params.insert("includeMessageCounts", "1".to_string());
        }
        // Counting messages scans every session behind each roster entry: that is
        // exactly the case the published read budget must not apply to.
        if include_message_counts {
            self.get_json_on(&self.http_long, "/api/v1/group-members", &params).await
        } else {
            self.get_json("/api/v1/group-members", &params).await
        }
    }

    /// `GET /api/v1/media/{id}` — bytes for a handle the server advertised.
    ///
    /// `id` is a **single path segment**: the native face's `mediaId`, or the
    /// last segment of `media.url` / `media.mediaUrl`. Prefer
    /// [`Client::media_bytes`] when a ChatLab message is at hand — that one
    /// also triggers an export and retries once on a 404.
    pub async fn media_bytes_by_id(&self, id: &str) -> Result<bytes::Bytes> {
        if id.trim().is_empty() {
            return Err(ClientError::UnexpectedBody {
                url: self.url("/api/v1/media"),
                detail: "media id must not be empty".to_string(),
            });
        }
        let url = self.url(&format!("/api/v1/media/{}", encode_path_segment(id)));
        let resp = self.http_long.get(&url).bearer_auth(&self.token).send().await?;
        Self::decode_bytes(resp, &url).await
    }

    // ---- watch ------------------------------------------------------------

    /// Subscribe to live events.
    ///
    /// Reconnects with `Last-Event-ID` on stream errors with capped backoff;
    /// heartbeat comment frames are skipped; the decoded events carry the
    /// server's generation counter so the caller can fall back to
    /// [`Client::drain_session`] when it jumps - the replay buffer cannot
    /// cover a generation switch by design.
    pub fn watch(
        &self,
    ) -> impl futures_util::Stream<Item = Result<ServerEvent>> + Send {
        let client = self.clone();
        async_stream::stream! {
            let mut sse = SseFrameState::default();
            let mut backoff = Duration::from_millis(500);
            loop {
                // 每条连接开始时丢弃上一连接遗留的暂存 id：它属于那条连接上一个
                // 从未出现 data: 的帧，跨连接提交会把未交付事件的 id 写进重放游标。
                sse.begin_connection();
                let url = client.url("/api/v1/push/messages");
                let mut req = client
                    .http_long
                    .get(&url)
                    .bearer_auth(&client.token)
                    .header("accept", "text/event-stream");
                if let Some(id) = sse.last_event_id {
                    req = req.header("last-event-id", id.to_string());
                }
                let resp = match req.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        yield Err(e.into());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        continue;
                    }
                };
                if !resp.status().is_success() {
                    yield Err(ClientError::Status { status: resp.status().as_u16(), url });
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
                backoff = Duration::from_millis(500);
                let mut stream = resp.bytes_stream();
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match stream.next().await {
                        Some(Ok(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                                let line = String::from_utf8_lossy(&buf[..pos]).to_string();
                                buf.drain(..=pos);
                                if let Some(event) = sse.feed(&line) {
                                    yield event;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            yield Err(e.into());
                            break;
                        }
                        None => break,
                    }
                }
                // Stream ended; reconnect after a pause. The loop carries
                // `last_event_id` so the replay window fills the gap.
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// SSE 的行级解析状态：`id:` 与它所属事件的游标。
///
/// 服务器的帧是 `id:` / `event:` / `data:`（JSON 载荷在 `data:` 行），`:` 开头是心跳注释。
/// `id:` 只**暂存**、不立刻推进重放游标 —— 它属于"接下来那一个事件"。断线恰好落在
/// `id:` 与 `data:` 之间时，那个 id 指向一个**从未交付给调用方**的事件：若已经推进游标，
/// 重连就会让服务端把那一条从重放窗口里划掉，事件静默丢失。
#[derive(Default)]
struct SseFrameState {
    /// `id:` 行的暂存值；见到 `data:`（帧已被消费）时提交给 `last_event_id`
    pending_id: Option<u64>,
    /// 重连用的游标
    last_event_id: Option<u64>,
}

impl SseFrameState {
    /// 一条新连接开始：丢弃上一连接遗留的暂存 id（`last_event_id` 是跨连接携带的
    /// 重放游标，必须保留）。
    fn begin_connection(&mut self) {
        self.pending_id = None;
    }

    /// 喂一行；解出一个完整帧时返回要交给调用方的那一项。
    fn feed(&mut self, line: &str) -> Option<Result<ServerEvent>> {
        if let Some(rest) = line.strip_prefix("id:") {
            if let Ok(id) = rest.trim().parse::<u64>() {
                self.pending_id = Some(id);
            }
            return None;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // 提交点在这里：出现 data: 就说明这一帧已被消费（无论解出事件还是解码错误）。
            // 若只在成功交付时提交，一个永久解不开的帧会让游标停在它前一步，每次重连都重放
            // 同一帧 —— 整条流被毒住。
            if let Some(id) = self.pending_id.take() {
                self.last_event_id = Some(id);
            }
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            // The notification face's frames are meta-only; decode by `event` key
            // inside the payload (the SSE `event:` line is set to the same value, but
            // decoding from the payload keeps one source of truth).
            let v: serde_json::Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(e) => return Some(Err(e.into())),
            };
            let kind = v.get("event").and_then(|e| e.as_str()).unwrap_or("");
            return Some(decode_event(kind, &v));
        }
        None
    }
}fn decode_event(kind: &str, v: &serde_json::Value) -> Result<ServerEvent> {
    match kind {
        "message.new" => Ok(ServerEvent::New(serde_json::from_value(v.clone())?)),
        "message.revoke" => Ok(ServerEvent::Revoke(serde_json::from_value(v.clone())?)),
        "sync" => Ok(ServerEvent::Sync(serde_json::from_value(v.clone())?)),
        "notification" => Ok(ServerEvent::Notification(serde_json::from_value(v.clone())?)),
        other => {
            // Unknown event kinds must not kill the stream: log and skip.
            log::debug!("ignoring unknown SSE event kind: {other}");
            Ok(ServerEvent::Notification(gen_types::NotificationFrame {
                event: other.to_string(),
                event_id: String::new(),
                platform_message_id: None,
                session_id: String::new(),
                timestamp: 0,
            }))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_escapes_delimiters_but_keeps_real_id_shapes() {
        // 真实形态原样保留：wxid 的 `@`、群号的 `-`、下划线与点
        assert_eq!(encode_path_segment("12345678@chatroom"), "12345678@chatroom");
        assert_eq!(encode_path_segment("wxid_ab-c.d~e"), "wxid_ab-c.d~e");
        // URL 定界符必须编码：否则 `a#b` 会被当成片段、`a?b` 会被当成查询，请求打到别的路径
        assert_eq!(encode_path_segment("a#b"), "a%23b");
        assert_eq!(encode_path_segment("a?b"), "a%3Fb");
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(encode_path_segment("中文"), "%E4%B8%AD%E6%96%87");
    }

    /// 悬空的 `id:` 不得跨连接泄漏：新连接的首帧若不带自己的 id（SSE 允许省略），提交来的
    /// 会是上一连接遗留的暂存值 —— 那条事件从未交付，游标一推进它就会从重放窗口消失。
    #[test]
    fn pending_id_does_not_leak_across_connections() {
        let mut sse = SseFrameState::default();
        // 连接 1：只等到 id: 8，没等到 data: 就断了
        sse.begin_connection();
        assert!(sse.feed("id: 8").is_none());
        sse.begin_connection();
        // 连接 2 的首帧不带 id（省略）
        let payload = serde_json::json!({
            "event": "message.new", "rawid": "9", "sessionId": "alice",
            "sessionType": "chat", "sourceName": "alice", "timestamp": 1,
            "content": "hi",
        });
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        assert!(sse.feed(&line).is_some(), "帧本身仍应交付");
        assert_eq!(
            sse.last_event_id, None,
            "上一连接遗留的 pending id 不得提交进重放游标",
        );
        // 同一连接内的正常序列必须提交
        assert!(sse.feed("id: 12").is_none());
        // 提交点必须唯一地落在 data: —— 在 event: 行提交是同一类缺陷的另一种写法，
        // 只有 id 与 event、永远等不到 data 的帧会让游标越过未交付事件。
        assert!(sse.feed("event: message.new").is_none());
        assert_eq!(sse.last_event_id, None, "event: 行不得提交游标");
        assert!(sse.feed(&line).is_some());
        assert_eq!(sse.last_event_id, Some(12), "同连接内 id 先于 data 必须推进游标");
    }

    #[test]
    fn published_timeouts_match_the_documented_budgets() {
        // reqwest exposes no accessor for a built Client's configuration, so the
        // wire behaviour of the two budgets cannot be asserted from Rust the way
        // Python can (its transport sees the per-request timeout). What this can
        // pin is the published numbers themselves: they are the contract a caller
        // compares against, and a silent edit here would make the docs a lie.
        // The routing (which request gets which client) is covered on the Python
        // side by test_published_timeout_budgets_travel_per_request.
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(READ_TIMEOUT, Duration::from_secs(30));
    }
    #[test]
    fn empty_talker_error_names_the_endpoint_that_was_actually_called() {
        let q = MessageQuery::new("");
        let msg = format!("{}", q.params("/chatlab/messages").unwrap_err());
        assert!(msg.contains("/chatlab/messages"), "报错要指向真正被调用的端点: {msg}");
        assert!(!msg.contains("/api/v1/messages"), "不该指向另一个端点: {msg}");
    }
}