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
    /// `search` was given a malformed date bound.
    #[error("invalid date bound {field}={value:?}: expected YYYYMMDD")]
    BadDate {
        /// Which parameter.
        field: &'static str,
        /// What was passed.
        value: String,
    },
}

/// Result alias for the behavior layer.
pub type Result<T> = std::result::Result<T, ClientError>;

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
        let url = self.url("/api/v1/accounts");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Status { status: status.as_u16(), url });
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_state;
        loop {
            let accounts: gen_types::AccountsList =
                self.get_json("/api/v1/accounts", &BTreeMap::new()).await?;
            let mine = accounts.accounts.iter().find(|a| a.wxid == wxid);
            match mine {
                Some(a) if a.state == gen_types::AccountStatus::Ready => return Ok(()),
                Some(a) if a.state == gen_types::AccountStatus::Error => {
                    return Err(ClientError::NotReady {
                        timeout,
                        last_state: format!("error: {}", a.error.clone().unwrap_or_default()),
                    });
                }
                Some(a) => last_state = a.state.to_string(),
                None => last_state = "not-registered".into(),
            }
            let _ = &last_state;
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::NotReady { timeout, last_state });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    // ---- drain_session ---------------------------------------------------

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
            let mut q = BTreeMap::new();
            if let Some(s) = next_since {
                q.insert("since", s.to_string());
            }
            if next_offset != 0 {
                q.insert("offset", next_offset.to_string());
            }
            let page: gen_types::PullEnvelope = self
                .get_json(
                    &format!("/api/v1/sessions/{talker}/messages"),
                    &q,
                )
                .await?;
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
    pub async fn list_all_sessions(&self) -> Result<Vec<gen_types::SessionNative>> {
        let mut out: Vec<gen_types::SessionNative> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut offset = 0usize;
        loop {
            let mut q = BTreeMap::new();
            q.insert("offset", offset.to_string());
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

    // ---- search ------------------------------------------------------------

    /// Keyword + time-window search over the native messages face.
    ///
    /// `YYYYMMDD` bounds are validated client-side with the same semantics as
    /// the server: `end` covers the whole day.
    pub async fn search(
        &self,
        talker: &str,
        keyword: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<gen_types::MessagesNative> {
        for (field, value) in [("start", start), ("end", end)] {
            if let Some(v) = value {
                self.validate_date(field, v)?;
            }
        }
        let mut q = BTreeMap::new();
        q.insert("talker", talker.to_string());
        q.insert("keyword", keyword.to_string());
        if let Some(s) = start {
            q.insert("start", s.to_string());
        }
        if let Some(e) = end {
            q.insert("end", e.to_string());
        }
        self.get_json("/api/v1/messages", &q).await
    }

    fn validate_date(&self, field: &'static str, value: &str) -> Result<()> {
        let ok = value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit());
        if ok {
            Ok(())
        } else {
            Err(ClientError::BadDate { field, value: value.to_string() })
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
        let url_path = format!("/api/v1/media/{name}");
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