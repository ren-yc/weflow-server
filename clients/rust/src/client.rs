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

/// Client for one weflow-server instance. Cloneable; shares the connection
/// pool.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Client {
    /// Point a client at a server. `token` is the API token the server
    /// printed on first start (or `--show-token`).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
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
        let url = self.url(path);
        let resp = self
            .http
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
        Ok(resp.json::<T>().await?)
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
        let resp = self.http.post(&url).bearer_auth(&self.token).send().await?;
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
        loop {
            let accounts = self.accounts().await?;
            let last_state = match accounts.iter().find(|a| a.wxid == wxid) {
                Some(a) if a.state == gen_types::AccountStatus::Ready => return Ok(()),
                Some(a) if a.state == gen_types::AccountStatus::Error => {
                    return Err(ClientError::NotReady {
                        timeout,
                        last_state: format!("error: {}", a.error.clone().unwrap_or_default()),
                    });
                }
                Some(a) => a.state.to_string(),
                None => "not-registered".to_string(),
            };
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
    pub async fn media_bytes(&self, message: &gen_types::ChatlabMessage) -> Result<bytes::Bytes> {
        let Some(m) = &message.media else {
            return Err(ClientError::Status { status: 404, url: "(no media on message)".into() });
        };
        let name = &m.file_name;
        let url_path = format!("/api/v1/media/{}", encode_path_segment(name));
        let url = self.url(&url_path);
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Trigger export, then retry once.
            let mut q = BTreeMap::new();
            q.insert("talker", message.account_name.clone());
            q.insert("media", "1".to_string());
            let _: gen_types::ChatlabMessages = self
                .get_json("/chatlab/messages", &q)
                .await?;
            let retry = self.http.get(&url).bearer_auth(&self.token).send().await?;
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
        let mut params = BTreeMap::new();
        params.insert("chatroomId", chatroom.to_string());
        if include_message_counts {
            params.insert("includeMessageCounts", "1".to_string());
        }
        self.get_json("/api/v1/group-members", &params).await
    }

    /// `GET /api/v1/media/{id}` — bytes for a handle the server advertised.
    ///
    /// `id` is a **single path segment**: the native face's `mediaId`, or the
    /// last segment of `media.url` / `media.mediaUrl`. Prefer
    /// [`Client::media_bytes`] when a ChatLab message is at hand — that one
    /// also triggers an export and retries once on a 404.
    pub async fn media_bytes_by_id(&self, id: &str) -> Result<bytes::Bytes> {
        let url = self.url(&format!("/api/v1/media/{}", encode_path_segment(id)));
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;
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
            let mut last_event_id: Option<u64> = None;
            let mut backoff = Duration::from_millis(500);
            loop {
                let url = client.url("/api/v1/push/messages");
                let mut req = client.http.get(&url).bearer_auth(&client.token).header("accept", "text/event-stream");
                if let Some(id) = last_event_id {
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
                                if let Some(event) = parse_sse_line(&line, &mut last_event_id) {
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

/// Parse one SSE line; returns a decoded event when a complete frame landed.
/// The server's frames are `id:` / `event:` / `data:` with the JSON payload on
/// the `data:` line; `:`-prefixed comment lines are heartbeats.
fn parse_sse_line(line: &str, last_event_id: &mut Option<u64>) -> Option<Result<ServerEvent>> {
    if let Some(rest) = line.strip_prefix("id:") {
        if let Ok(id) = rest.trim().parse::<u64>() {
            *last_event_id = Some(id);
        }
        return None;
    }
    if let Some(rest) = line.strip_prefix("data:") {
        let payload = rest.strip_prefix(' ').unwrap_or(rest);
        // The notification face's frames are meta-only; decode by `event` key
        // inside the payload (the SSE `event:` line is set to the same value,
        // but decoding from the payload keeps one source of truth).
        let v: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => return Some(Err(e.into())),
        };
        let kind = v.get("event").and_then(|e| e.as_str()).unwrap_or("");
        return Some(decode_event(kind, &v));
    }
    None
}

fn decode_event(kind: &str, v: &serde_json::Value) -> Result<ServerEvent> {
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

    #[test]
    fn empty_talker_error_names_the_endpoint_that_was_actually_called() {
        let q = MessageQuery::new("");
        let msg = format!("{}", q.params("/chatlab/messages").unwrap_err());
        assert!(msg.contains("/chatlab/messages"), "报错要指向真正被调用的端点: {msg}");
        assert!(!msg.contains("/api/v1/messages"), "不该指向另一个端点: {msg}");
    }
}