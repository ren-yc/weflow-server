//! Behavior-layer tests against an in-process axum mock that mirrors the
//! server's wire shapes. No real database: the fixtures here emit the same
//! JSON the server's golden snapshots pin, which is the contract the client
//! codes against.

use std::sync::Mutex as StdMutex;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use weflow_client::client::{Client, ClientError, ContactsQuery, MessageQuery, ServerEvent};

const TOKEN: &str = "test-token-0123456789abcdef";

#[derive(Clone, Default)]
struct Mock {
    states: Arc<StdMutex<Vec<String>>>,
    pull_pages: Arc<StdMutex<Vec<serde_json::Value>>>,
    pull_queries: Arc<StdMutex<Vec<String>>>,
    media_calls: Arc<StdMutex<Vec<String>>>,
    media_bytes: Arc<StdMutex<Option<Vec<u8>>>>,
    chatlab_page: Arc<StdMutex<Option<serde_json::Value>>>,
    // When set, the chatlab export route rejects any query whose `talker`
    // differs - the retry must ask for the session id, not
    // the display name.
    chatlab_query_talker: Arc<StdMutex<Option<String>>>,
    sse_frames: Arc<StdMutex<Vec<String>>>,
    sse_event: Arc<StdMutex<Option<serde_json::Value>>>,
    sse_reconnect_ids: Arc<StdMutex<Vec<Option<String>>>>,
    /// When set, the health route answers this body verbatim (wrong-shape 反例用)。
    health_page: Arc<StdMutex<Option<serde_json::Value>>>,
    /// Frames served per connection, in order (popped at request time); when
    /// exhausted the fixed sse_frames body is used.
    sse_frames_seq: Arc<StdMutex<std::collections::VecDeque<Vec<String>>>>,
    messages_query: Arc<StdMutex<Option<String>>>,
    /// When set, the accounts route answers `indexing` forever (timeout test).
    always_indexing: bool,
    /// When set, `GET /api/v1/accounts` answers this page verbatim.
    accounts_page: Arc<StdMutex<Option<serde_json::Value>>>,
    /// How many times the accounts route was hit (asserts "no polling").
    accounts_calls: Arc<StdMutex<usize>>,
    /// When set, `POST /api/v1/accounts` answers this body verbatim.
    register_response: Arc<StdMutex<Option<serde_json::Value>>>,
    /// How many times the register route was hit (asserts "no registration").
    post_calls: Arc<StdMutex<usize>>,
    /// When set, `GET /api/v1/messages` answers this page verbatim.
    native_page: Arc<StdMutex<Option<serde_json::Value>>>,
    /// Query strings seen by the native messages route, in order.
    native_queries: Arc<StdMutex<Vec<String>>>,
    /// When set, `GET /api/v1/contacts` answers this page verbatim.
    contacts_page: Arc<StdMutex<Option<serde_json::Value>>>,
    /// When set, `GET /api/v1/group-members` answers this page verbatim.
    group_members_page: Arc<StdMutex<Option<serde_json::Value>>>,
    /// Query strings seen by the group-members route, in order.
    group_members_queries: Arc<StdMutex<Vec<String>>>,
    /// FIFO pages for `GET /api/v1/sessions`.
    sessions_pages: Arc<StdMutex<Vec<serde_json::Value>>>,
    /// Query strings seen by the sessions route, in order.
    sessions_queries: Arc<StdMutex<Vec<String>>>,
    /// How many times `/health` was hit, and whether it carried credentials.
    health_calls: Arc<StdMutex<Vec<bool>>>,
    /// How many times `POST /api/v1/sync` was hit (asserts "no polling").
    sync_calls: Arc<StdMutex<usize>>,
}

fn parse_query(q: &Option<String>) -> Vec<(String, String)> {
    q.as_deref()
        .unwrap_or("")
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('?').unwrap_or(kv.split_once('=').unwrap_or((kv, "")));
            (k.to_string(), v.to_string())
        })
        .collect()
}

async fn spawn_mock(mock: Mock) -> String {
    let app = Router::new()
        .route("/api/v1/accounts", post(accounts_post).get(accounts_get))
        .route("/api/v1/sessions/{id}/messages", get(pull_route))
        .route("/chatlab/messages", get(chatlab_route))
        .route("/api/v1/media/{id}", get(media_route))
        .route("/api/v1/messages", get(messages_route))
        .route("/api/v1/contacts", get(contacts_route))
        .route("/api/v1/group-members", get(group_members_route))
        .route("/api/v1/sessions", get(sessions_route))
        .route("/api/v1/sync", post(sync_post))
        .route("/health", get(health_route))
        .route("/api/v1/push/messages", get(sse_route))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn accounts_post(
    State(mock): State<Mock>,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    *mock.post_calls.lock().unwrap() += 1;
    let body = mock
        .register_response
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| serde_json::json!({"success": true, "state": "indexing"}));
    Json(body).into_response()
}

/// `POST /api/v1/sync` — counts calls and requires the bearer token.
///
/// The counter is the point: the SDK must trigger a sync **only** when the
/// caller asked for one, never from a polling path.
async fn sync_post(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    assert_bearer(&headers);
    *mock.sync_calls.lock().unwrap() += 1;
    Json(serde_json::json!({
        "success": true,
        "newMessages": 7,
        "revokeMessages": 2,
    }))
    .into_response()
}

fn assert_bearer(headers: &HeaderMap) {
    let got = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(got, format!("Bearer {TOKEN}"), "auth must go in the Authorization header");
}

async fn accounts_get(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    assert_bearer(&headers);
    *mock.accounts_calls.lock().unwrap() += 1;
    if let Some(page) = mock.accounts_page.lock().unwrap().clone() {
        return Json(page).into_response();
    }
    let state = if mock.always_indexing {
        "indexing".to_string()
    } else {
        mock.states.lock().unwrap().pop().unwrap_or_else(|| "ready".to_string())
    };
    Json(serde_json::json!({
        "success": true,
        "accounts": [{
            "wxid": "wxid_mock", "db_storage": "", "message_count": 1,
            "state": state,
        }],
    }))
    .into_response()
}

async fn pull_route(
    State(mock): State<Mock>,
    Path(id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    mock.pull_queries.lock().unwrap().push(format!("{id}?{}", query.unwrap_or_default()));
    // FIFO: pages are listed in request order, so take from the front.
    let page = {
        let mut q = mock.pull_pages.lock().unwrap();
        if q.is_empty() { None } else { Some(q.remove(0)) }
    };
    match page {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "fixture exhausted").into_response(),
    }
}

async fn chatlab_route(
    State(mock): State<Mock>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    *mock.messages_query.lock().unwrap() = query.clone();
    if let Some(expected) = mock.chatlab_query_talker.lock().unwrap().clone() {
        let got = query.as_deref().unwrap_or("").split("&").find_map(|kv| {
            let (k, v) = kv.split_once("=")?;
            (k == "talker").then(|| v.to_string())
        });
        if got.as_deref() != Some(expected.as_str()) {
            return (StatusCode::BAD_REQUEST, format!("export asked with wrong talker: {got:?} (expected {expected:?})")).into_response();
        }
    }
    let page = mock.chatlab_page.lock().unwrap().clone();
    match page {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "fixture missing").into_response(),
    }
}

async fn media_route(
    State(mock): State<Mock>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    mock.media_calls.lock().unwrap().push(id);
    // First call misses, and the miss itself mints the file - modeling the
    // export that the client is about to trigger via the chatlab face.
    let bytes = {
        let mut slot = mock.media_bytes.lock().unwrap();
        match slot.take() {
            Some(b) => Some(b),
            None => {
                *slot = Some(b"png-bytes".to_vec());
                None
            }
        }
    };
    match bytes {
        Some(b) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], b).into_response(),
        None => (StatusCode::NOT_FOUND, "media not exported").into_response(),
    }
}

async fn messages_route(
    State(mock): State<Mock>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    *mock.messages_query.lock().unwrap() = query.clone();
    mock.native_queries.lock().unwrap().push(query.unwrap_or_default());
    if let Some(page) = mock.native_page.lock().unwrap().clone() {
        return Json(page).into_response();
    }
    // The native face is camelCase on the wire; the fallback must decode as
    // `MessagesNative` or every caller of this route looks like a shape break.
    Json(serde_json::json!({
        "success": true, "count": 0, "hasMore": false,
        "talker": "", "media": {"count": 0, "enabled": false, "exportPath": ""}, "messages": [],
    }))
    .into_response()
}

/// `/health` is unauthenticated: the mock records whether a bearer arrived,
/// so the test can pin "the SDK does not send credentials to it".
async fn health_route(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    let carried = headers.contains_key("authorization");
    mock.health_calls.lock().unwrap().push(carried);
    if let Some(page) = mock.health_page.lock().unwrap().clone() {
        return Json(page).into_response();
    }
    Json(serde_json::json!({"account": "ready", "status": "ok", "version": "9.9.9"}))
        .into_response()
}

async fn contacts_route(
    State(mock): State<Mock>,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    match mock.contacts_page.lock().unwrap().clone() {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "fixture missing").into_response(),
    }
}

async fn group_members_route(
    State(mock): State<Mock>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    mock.group_members_queries.lock().unwrap().push(query.unwrap_or_default());
    match mock.group_members_page.lock().unwrap().clone() {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "fixture missing").into_response(),
    }
}

async fn sessions_route(
    State(mock): State<Mock>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    mock.sessions_queries.lock().unwrap().push(query.unwrap_or_default());
    let page = {
        let mut q = mock.sessions_pages.lock().unwrap();
        if q.is_empty() { None } else { Some(q.remove(0)) }
    };
    match page {
        Some(p) => Json(p).into_response(),
        None => (StatusCode::NOT_FOUND, "fixture exhausted").into_response(),
    }
}

async fn sse_route(
    State(mock): State<Mock>,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).map(String::from);
    mock.sse_reconnect_ids.lock().unwrap().push(last_id);
    let seq = mock.sse_frames_seq.lock().unwrap().pop_front();
    let frames = match seq {
        Some(per_connection) => per_connection.join("\n"),
        None => mock.sse_frames.lock().unwrap().join("\n"),
    };
    let body = format!("{frames}\n");
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

fn mock_message_json(talker: &str, file: &str) -> serde_json::Value {
    serde_json::json!({
        "accountName": talker,
        "content": "hello",
        "groupNickname": "",
        "media": {"type": "image", "fileName": file, "md5": "abc"},
        "platformMessageId": "42",
        "sender": "alice",
        "timestamp": 1_700_000_000,
        "type": 1,
    })
}

// ---- ensure_ready -------------------------------------------------------

#[tokio::test]
async fn ensure_ready_polls_until_ready_and_absorbs_intermediate_states() {
    let mock = Mock::default();
    *mock.states.lock().unwrap() = vec!["indexing".into(), "indexing".into()];
    let base = spawn_mock(mock).await;
    let client = Client::new(&base, TOKEN);
    client
        .ensure_ready(
            "wxid_mock",
            &serde_json::json!({"wxid": "wxid_mock", "db_path": "X:/db"}),
            Duration::from_secs(5),
        )
        .await
        .expect("must reach ready through indexing");
}

#[tokio::test]
async fn ensure_ready_times_out_with_the_last_observed_state() {
    let mock = Mock::default();
    // Always indexing: the route answers the same state forever.
    let mut mock = mock;
    mock.always_indexing = true;
    let base = spawn_mock(mock).await;
    let client = Client::new(&base, TOKEN);
    let err = client
        .ensure_ready(
            "wxid_mock",
            &serde_json::json!({"wxid": "wxid_mock", "db_path": "X:/db"}),
            Duration::from_millis(700),
        )
        .await
        .expect_err("must time out");
    match err {
        ClientError::NotReady { last_state, .. } => assert_eq!(last_state, "indexing"),
        other => panic!("expected NotReady, got {other:?}"),
    }
}

// ---- drain_session -------------------------------------------------------

#[tokio::test]
async fn drain_session_echoes_cursors_verbatim_until_exhausted() {
    let mock = Mock::default();
    let msg = |id: u64, ts: i64| {
        serde_json::json!({
            "accountName": "wxid_mock", "content": format!("m{id}"), "groupNickname": "",
            "platformMessageId": id.to_string(), "sender": "alice", "timestamp": ts,
            "type": 1,
        })
    };
    // Page 1: hasMore + nextSince=1000 + nextOffset=7. Page 2: final.
    *mock.pull_pages.lock().unwrap() = vec![
        serde_json::json!({
            "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
            "members": [],
            "messages": [msg(1, 990), msg(2, 1000)],
            "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
            "sync": {"hasMore": true, "nextSince": 1000, "nextOffset": 7, "watermark": 2000},
        }),
        serde_json::json!({
            "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
            "members": [],
            "messages": [msg(3, 1500)],
            "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
            "sync": {"hasMore": false, "nextSince": 2000, "nextOffset": 0, "watermark": 2000},
        }),
    ];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);

    let mut pages: Vec<Vec<u64>> = Vec::new();
    let total = client
        .drain_session("alice", Some(500), |batch| {
            pages.push(batch.iter().map(|m| m.platform_message_id.parse().unwrap()).collect());
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(pages, vec![vec![1, 2], vec![3]]);

    let queries = mock.pull_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 2, "one request per page");
    assert!(queries[0].contains("alice?since=500"), "first request carries the requested since: {}", queries[0]);
    assert!(queries[1].contains("since=1000") && queries[1].contains("offset=7"),
        "second request echoes the server's cursors verbatim: {}", queries[1]);
}

// ---- pull_page -----------------------------------------------------------

#[tokio::test]
async fn pull_page_decodes_the_sync_block_and_sends_the_cursors() {
    let mock = Mock::default();
    *mock.pull_pages.lock().unwrap() = vec![serde_json::json!({
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "members": [],
        "messages": [{
            "accountName": "alice", "content": "m1", "groupNickname": "",
            "platformMessageId": "1", "sender": "alice", "timestamp": 1000, "type": 1,
        }],
        "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
        "sync": {"hasMore": true, "nextSince": 1000, "nextOffset": 4, "watermark": 2000},
    })];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);

    let page = client.pull_page("alice", Some(500), 7, Some(3)).await.unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].platform_message_id, "1");
    assert!(page.sync.has_more);
    assert_eq!(page.sync.next_since, 1000);
    assert_eq!(page.sync.next_offset, 4);
    assert_eq!(page.sync.watermark, 2000);

    let queries = mock.pull_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1, "one page is one request");
    assert!(queries[0].contains("since=500"), "since rides the query: {}", queries[0]);
    assert!(queries[0].contains("offset=7"), "the group cursor rides the query: {}", queries[0]);
    assert!(queries[0].contains("limit=3"), "the per-page cap rides the query: {}", queries[0]);
}

#[tokio::test]
async fn pull_page_omits_defaulted_cursors_instead_of_sending_zero() {
    let mock = Mock::default();
    *mock.pull_pages.lock().unwrap() = vec![serde_json::json!({
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "members": [],
        "messages": [],
        "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
        "sync": {"hasMore": false, "nextSince": 0, "nextOffset": 0, "watermark": 0},
    })];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);

    client.pull_page("alice", None, 0, None).await.unwrap();
    let queries = mock.pull_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    for key in ["since=", "offset=", "limit="] {
        assert!(!queries[0].contains(key),
            "{key} must be absent, not defaulted on the wire: {}", queries[0]);
    }
}

// ---- media_bytes ---------------------------------------------------------

#[tokio::test]
async fn media_bytes_exports_then_retries_once_after_404() {
    let mock = Mock::default();
    let talker = "wxid_mock";
    // The display name and the session id must DIFFER here: the old retry
    // used `message.account_name` as the export `talker`, and the fixture let
    // the two coincide so the mistake was invisible. The mock now rejects any
    // export query whose talker is not the session id we pass.
    let display_name = "Alice DISPLAY";
    *mock.chatlab_page.lock().unwrap() = Some(serde_json::json!({
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 0, "members": [], "messages": [],
        "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
        "page": {"hasMore": false, "nextCursor": null},
        "talker": talker,
    }));
    *mock.chatlab_query_talker.lock().unwrap() = Some(talker.to_string());
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let message: weflow_client::generated::r#gen::types::ChatlabMessage =
        serde_json::from_value(mock_message_json(display_name, "abc123.png"))
            .unwrap();
    assert_eq!(message.account_name, display_name, "fixture: display name differs from session id");
    let bytes = client.media_bytes(&message, talker).await.expect("retry must succeed");
    assert_eq!(bytes.as_ref(), b"png-bytes");
    let calls = mock.media_calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["abc123.png", "abc123.png"], "two GETs: miss then retry");
}

#[tokio::test]
async fn media_bytes_without_a_handle_is_unexpected_body_not_a_fake_404() {
    // The old code reported a synthesized 404 with a sentence in the url
    // field: a caller classifying by status saw a server answer that never
    // happened, and one matching on url saw prose. The local precondition is
    // its own kind of error.
    let mock = Mock::default();
    let base = spawn_mock(mock).await;
    let client = Client::new(&base, TOKEN);
    let message: weflow_client::generated::r#gen::types::ChatlabMessage =
        serde_json::from_value(serde_json::json!({
            "accountName": "alice", "content": "x", "groupNickname": "",
            "platformMessageId": "1", "sender": "alice",
            "timestamp": 1_700_000_000, "type": 1,
        }))
        .unwrap();
    match client.media_bytes(&message, "wxid_alice").await {
        Err(ClientError::UnexpectedBody { .. }) => {}
        other => panic!("expected UnexpectedBody, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_decodes_frames_and_reconnects_with_last_event_id() {
    let mock = Mock::default();
    let event = serde_json::json!({
        "event": "message.new", "rawid": "9", "sessionId": "alice",
        "sessionType": "chat", "sourceName": "alice", "timestamp": 1_700_000_001,
        "content": "hi",
    });
    *mock.sse_event.lock().unwrap() = Some(event);
    *mock.sse_frames.lock().unwrap() = vec![
        ": heartbeat".to_string(),
        "id: 7".to_string(),
        "event: message.new".to_string(),
        "data: {\"event\":\"message.new\",\"rawid\":\"9\",\"sessionId\":\"alice\",\"sessionType\":\"chat\",\"sourceName\":\"alice\",\"timestamp\":1700000001,\"content\":\"hi\"}".to_string(),
    ];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    use futures_util::StreamExt as _;
    let mut stream = Box::pin(client.watch());
    let first = stream.as_mut().next().await.expect("stream must yield").expect("decode");
    match &first {
        ServerEvent::New(e) => {
            assert_eq!(e.rawid, "9");
            assert_eq!(e.session_id, "alice");
        }
        other => panic!("expected New, got {other:?}"),
    }
    assert_eq!(first.kind(), "message.new");
    // The heartbeat comment must not have become an event; reconnect (stream
    // ended because the mock returns a fixed body) carries the last id.
    // The mock's SSE body ends after one frame; the next poll drives the
    // reconnect path (second HTTP call records the Last-Event-ID). Give the
    // background task a beat, then read the reconnect log.
    let _ = stream.as_mut().next().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reconnects = mock.sse_reconnect_ids.lock().unwrap().clone();
    assert!(
        reconnects.iter().any(|id| id.as_deref() == Some("7")),
        "reconnect must carry Last-Event-ID: 7, got {reconnects:?}"
    );
}


/// 断线恰好落在 `id:` 与 `data:` 之间时，那个 id 指向一个**从未交付给调用方**的事件。
///
/// 游标若在见到 `id:` 时就推进，重连会让服务端把这条从重放窗口里划掉 —— 事件静默丢失，
/// 而调用方永远不知道它存在过。这里先正常交付一帧（游标应推进到 7），再给一个只有 `id: 8`
/// 的悬空帧：重连必须仍带 7、绝不带 8。
#[tokio::test]
async fn watch_does_not_advance_the_cursor_past_an_undelivered_frame() {
    let mock = Mock::default();
    // data 行运行时序列化：源码里不出现带转义的字符串字面量，也不会与真实帧格式漂移。
    let payload = serde_json::json!({
        "event": "message.new", "rawid": "9", "sessionId": "alice",
        "sessionType": "chat", "sourceName": "alice", "timestamp": 1_700_000_001,
        "content": "hi",
    });
    let data_line = format!("data: {}", serde_json::to_string(&payload).unwrap());
    *mock.sse_frames.lock().unwrap() = vec![
        "id: 7".to_string(),
        "event: message.new".to_string(),
        data_line,
        // 悬空的第二帧：只有 id，没有 data
        "id: 8".to_string(),
    ];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    use futures_util::StreamExt as _;
    let mut stream = Box::pin(client.watch());
    stream.as_mut().next().await.expect("stream must yield").expect("decode");
    let _ = stream.as_mut().next().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reconnects = mock.sse_reconnect_ids.lock().unwrap().clone();
    assert!(
        reconnects.iter().any(|id| id.as_deref() == Some("7")),
        "已交付的帧必须推进游标: {reconnects:?}",
    );
    assert!(
        !reconnects.iter().any(|id| id.as_deref() == Some("8")),
        "悬空的 id: 8 不得推进游标（否则那条事件从重放窗口消失）: {reconnects:?}",
    );
}

/// 形状损坏不能冒充传输故障。
///
/// `resp.json::<T>()` 把**解码失败也包成 reqwest::Error**，于是「服务端答了个不合承诺形状的
/// 东西」被记成「网络断了」—— 调用方按变体分流时两边同时失去区分力（重试传输故障合理，
/// 重试形状错误不合理）。
#[tokio::test]
async fn a_200_with_the_wrong_shape_is_a_shape_error_not_a_transport_one() {
    let mock = Mock::default();
    // 合法的 JSON、错误的形状：数组而非对象
    *mock.health_page.lock().unwrap() = Some(serde_json::json!([]));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let err = client.health().await.expect_err("200 但形状不符必须报错");
    assert!(
        matches!(err, ClientError::Shape(..)),
        "形状损坏必须是 Shape，实际 {err:?}",
    );
    assert!(
        !matches!(err, ClientError::Transport(..)),
        "不得记成传输故障（连接没坏，是服务端答错了）: {err:?}",
    );
}

/// 跨连接泄漏在**真实流**上的钉法：连接 2 的首帧故意不带 `id:` —— 若上一连接遗留的
/// 暂存 id 没有在连接开始时丢弃，它会在这一帧被提交进游标，第三条连接的重连头就变成
/// 那条从未交付事件的 id（服务端据此把它划出重放窗口）。单元测试只钉状态机，本条钉接线。
#[tokio::test]
async fn watch_resets_the_pending_id_when_a_new_connection_starts() {
    let mock = Mock::default();
    let frame = |rawid: &str| {
        format!(
            "data: {}",
            serde_json::to_string(&serde_json::json!({
                "event": "message.new", "rawid": rawid, "sessionId": "alice",
                "sessionType": "chat", "sourceName": "alice", "timestamp": 1_700_000_001,
                "content": "hi",
            }))
            .unwrap()
        )
    };
    *mock.sse_frames_seq.lock().unwrap() = vec![
        // 连接 1：交付一条（游标 → 7），随后断在悬空的 id: 8 上（始终没有 data:）
        vec![
            "id: 7".to_string(),
            "event: message.new".to_string(),
            frame("9"),
            "id: 8".to_string(),
        ],
        // 连接 2：首帧没有自己的 id
        vec![frame("10")],
        // 连接 3：只为读取它的重连头
        vec![frame("11")],
    ].into();
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    use futures_util::StreamExt as _;
    let mut stream = Box::pin(client.watch());
    // 三次 yield ≈ 三条连接（EOF 后 500ms 重连由 next().await 等待，不靠裸 sleep）
    for _ in 0..3 {
        let _ = stream.as_mut().next().await;
    }
    let heads = mock.sse_reconnect_ids.lock().unwrap().clone();
    assert_eq!(heads.len(), 3, "三条连接各记录一次重连头: {heads:?}");
    assert_eq!(heads[0], None, "首连没有游标");
    assert_eq!(heads[1].as_deref(), Some("7"), "悬空 id 不得推进：连接 2 仍带 7");
    assert_eq!(heads[2].as_deref(), Some("7"), "遗留 pending 不得跨连接提交: {heads:?}");
}
// ---- health / accounts / register / wait_ready ---------------------------

#[tokio::test]
async fn health_reports_version_and_account_phase() {
    let mock = Mock::default();
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let h = client.health().await.expect("health must decode");
    assert_eq!(h.version, "9.9.9");
    assert_eq!(h.status, "ok");
    assert_eq!(
        serde_json::to_string(&h.account).unwrap(),
        "\"ready\"",
        "the scalar phase is the whole point of this endpoint"
    );
    assert_eq!(
        mock.health_calls.lock().unwrap().clone(),
        vec![false],
        "/health is unauthenticated: the SDK must not send credentials to it"
    );
}

#[tokio::test]
async fn accounts_expose_state_error_and_message_count() {
    let mock = Mock::default();
    *mock.accounts_page.lock().unwrap() = Some(serde_json::json!({
        "success": true,
        "accounts": [
            {"wxid": "wxid_a", "db_storage": "X:/a", "message_count": 12, "state": "ready"},
            {"wxid": "wxid_b", "db_storage": "X:/b", "message_count": 0, "state": "error",
             "error": "bad key"},
        ],
    }));
    let base = spawn_mock(mock).await;
    let client = Client::new(&base, TOKEN);
    let accounts = client.accounts().await.expect("accounts must decode");
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].message_count, 12);
    assert_eq!(
        accounts[1].error.as_deref(),
        Some("bad key"),
        "the failure reason exists only on this face, not on /health"
    );
}

#[tokio::test]
async fn register_returns_raw_state_without_polling() {
    let mock = Mock::default();
    *mock.register_response.lock().unwrap() = Some(serde_json::json!({
        "success": false, "state": "account_conflict",
        "occupied_by": "wxid_other", "occupied_status": "ready",
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let outcome = client
        .register(&serde_json::json!({"wxid": "wxid_mock", "db_path": "X:/db"}))
        .await
        .expect("a refusal is a value, not an error");
    assert_eq!(outcome.state, "account_conflict");
    assert_eq!(outcome.status, None, "this state carries no status");
    assert_eq!(
        outcome.body["occupied_by"], "wxid_other",
        "refusal extras stay reachable in the body"
    );
    assert_eq!(*mock.post_calls.lock().unwrap(), 1, "one POST, no retry");
    assert_eq!(
        *mock.accounts_calls.lock().unwrap(),
        0,
        "register must not poll: waiting is wait_ready's job"
    );
}

#[tokio::test]
async fn wait_ready_polls_without_registering() {
    let mock = Mock::default();
    *mock.states.lock().unwrap() = vec!["indexing".into(), "indexing".into()];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    client
        .wait_ready("wxid_mock", Duration::from_secs(5))
        .await
        .expect("must reach ready through indexing");
    let polls = *mock.accounts_calls.lock().unwrap();
    assert!(polls >= 3, "it polls the listing until ready, saw {polls}");
    assert_eq!(*mock.post_calls.lock().unwrap(), 0, "wait_ready is wait-only");
}

// ---- list_messages / contacts / media_bytes_by_id ------------------------

#[tokio::test]
async fn list_messages_pages_by_offset_and_exposes_native_fields() {
    let mock = Mock::default();
    *mock.native_page.lock().unwrap() = Some(serde_json::json!({
        "success": true, "count": 1, "hasMore": true, "talker": "alice",
        "media": {"count": 0, "enabled": false, "exportPath": "X:/export"},
        "messages": [{
            "appmsgSubtype": null, "baseType": 1, "content": "hi",
            "createTime": 1_700_000_000, "isSend": 1, "localId": 7, "localType": 3,
            "media": {"fileName": "abc.png", "mediaId": "abc123", "md5": "d41d8", "type": "image"},
            "parsedContent": "hi", "quote": null, "rawContent": "<msg>hi</msg>",
            "replyToMessageId": "41", "senderName": "张三", "senderUsername": "alice",
            "serverId": "42", "sortSeq": 1,
        }],
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let mut q = MessageQuery::new("alice");
    q.limit = Some(500);
    q.offset = Some(1000);
    q.media = true;
    let page = client.list_messages(&q).await.expect("native page must decode");
    assert!(page.has_more, "paging continues until has_more is false");
    assert_eq!(page.media.export_path, "X:/export");
    let m = &page.messages[0];
    assert_eq!(m.raw_content, "<msg>hi</msg>", "the ChatLab shape drops rawContent");
    assert_eq!(m.is_send, 1);
    assert_eq!(m.local_type, 3);
    assert_eq!(
        m.media.as_ref().and_then(|x| x.media_id.as_deref()),
        Some("abc123"),
        "the fetchable handle rides inside media"
    );
    let queries = mock.native_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    let pairs = parse_query(&Some(queries[0].clone()));
    for want in [("talker", "alice"), ("limit", "500"), ("offset", "1000"), ("media", "1")] {
        assert!(
            pairs.iter().any(|(k, v)| k == want.0 && v == want.1),
            "query must carry {want:?}: {}",
            queries[0]
        );
    }
}

#[tokio::test]
async fn chatlab_messages_decodes_the_chatlab_envelope_and_paging() {
    let mock = Mock::default();
    *mock.chatlab_page.lock().unwrap() = Some(serde_json::json!({
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 1,
        "members": [{
            "accountName": "张三", "avatar": "", "groupNickname": "",
            "platformId": "alice", "username": "alice",
        }],
        "messages": [{
            "accountName": "alice", "content": "hi", "groupNickname": "",
            "platformMessageId": "42", "sender": "alice", "timestamp": 1700000000,
            "type": 1,
        }],
        "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "private"},
        "page": {"hasMore": true, "nextCursor": "1000"},
        "talker": "alice",
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let mut q = MessageQuery::new("alice");
    q.keyword = Some("hi".to_string());
    q.limit = Some(50);

    let page = client.chatlab_messages(&q).await.expect("chatlab page must decode");
    assert_eq!(page.count, 1);
    assert!(page.page.has_more);
    assert_eq!(page.page.next_cursor.as_deref(), Some("1000"));
    assert_eq!(page.messages[0].platform_message_id, "42");
    assert_eq!(page.messages[0].type_, 1, "ChatLab type codes, not the native localType");

    let recorded = mock.messages_query.lock().unwrap().clone();
    let pairs = parse_query(&recorded);
    for want in [("talker", "alice"), ("keyword", "hi"), ("limit", "50")] {
        assert!(
            pairs.iter().any(|(k, v)| k == want.0 && v == want.1),
            "query must carry {want:?}: {recorded:?}"
        );
    }
}
#[tokio::test]
async fn contacts_page_decodes_rows_and_paging_fields() {
    let mock = Mock::default();
    *mock.contacts_page.lock().unwrap() = Some(serde_json::json!({
        "success": true, "count": 1, "total": 42, "hasMore": true,
        "contacts": [{
            "alias": "", "avatarUrl": "", "displayName": "张三",
            "nickname": "三儿", "remark": "客户张三", "type": "friend", "username": "alice",
        }],
    }));
    let base = spawn_mock(mock).await;
    let client = Client::new(&base, TOKEN);
    let page = client
        .contacts(&ContactsQuery { limit: Some(100), offset: Some(0), keyword: None })
        .await
        .expect("contacts must decode");
    assert_eq!(page.count, 1);
    assert_eq!(page.total, 42);
    assert!(page.has_more);
    assert_eq!(page.contacts[0].display_name, "张三");
}

#[tokio::test]
async fn group_members_decodes_roster_page_and_sends_chatroom_param() {
    let mock = Mock::default();
    *mock.group_members_page.lock().unwrap() = Some(serde_json::json!({
        "success": true, "chatroomId": "123@chatroom", "count": 2, "fromCache": false,
        "updatedAt": 1_700_000_000_123i64,
        "members": [
            { "alias": "", "avatarUrl": "", "displayName": "潜水者", "groupNickname": "",
              "isFriend": false, "isOwner": false, "messageCount": 0,
              "nickname": "", "remark": "", "wxid": "quiet" },
            { "alias": "a", "avatarUrl": "", "displayName": "张三", "groupNickname": "张三",
              "isFriend": true, "isOwner": true, "messageCount": 9,
              "nickname": "三儿", "remark": "客户张三", "wxid": "alice" }
        ],
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let page = client
        .group_members("123@chatroom", true)
        .await
        .expect("group members must decode");
    assert_eq!(page.count, 2);
    assert_eq!(page.updated_at, 1_700_000_000_123, "updatedAt is **milliseconds** — a seconds truncation silently halves freshness precision");
    // The roster includes silent members: a zero-count row is legal, not an error.
    assert_eq!(page.members[0].message_count, 0);
    assert!(page.members[1].is_owner, "exactly one owner when the roster carries one");
    let queries = mock.group_members_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1, "exactly one GET /api/v1/group-members");
    assert!(queries[0].contains("chatroomId=123%40chatroom"), "the chatroom rides as a query param: {}", queries[0]);
    assert!(queries[0].contains("includeMessageCounts=1"), "counts asked for: {}", queries[0]);
}

/// 空群号问的是另一个问题：服务端会答成一个空名册，于是「这个群没有成员」与「你没告诉我是
/// 哪个群」在调用方看来一模一样。必须本地拒绝，并且**不发请求**。
#[tokio::test]
async fn group_members_rejects_an_empty_chatroom_without_a_request() {
    let mock = Mock::default();
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let err = client
        .group_members("", true)
        .await
        .expect_err("空群号必须本地拒绝");
    let msg = format!("{err}");
    assert!(msg.contains("chatroom must not be empty"), "报错要指出问题: {msg}");
    assert!(
        mock.group_members_queries.lock().unwrap().is_empty(),
        "本地校验失败时不该发出任何请求",
    );
}
/// The off switch: `include_message_counts = false` must **omit the parameter**
/// rather than send `0` — the server reads it through a flexible bool parser,
/// and the wire shape for "don't scan the conversation" is absence.
#[tokio::test]
async fn group_members_omits_include_message_counts_when_false() {
    let mock = Mock::default();
    *mock.group_members_page.lock().unwrap() = Some(serde_json::json!({
        "success": true, "chatroomId": "123@chatroom", "count": 0, "fromCache": false,
        "updatedAt": 0, "members": [],
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    client.group_members("123@chatroom", false).await.expect("empty page decodes");
    let queries = mock.group_members_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    assert!(!queries[0].contains("includeMessageCounts"), "absent, not 0: {}", queries[0]);
}

#[tokio::test]
async fn list_all_sessions_pages_and_collapses_cross_page_duplicates() {
    let mock = Mock::default();
    let sess = |u: &str| {
        serde_json::json!({
            "displayName": u, "lastTimestamp": 1, "messageCount": 0,
            "sessionType": "private", "summary": null, "type": 0,
            "unreadCount": 0, "username": u,
        })
    };
    // A live list can shift between pages: "b" shows up twice. The loop stops
    // on an empty page (this face has no has_more).
    *mock.sessions_pages.lock().unwrap() = vec![
        serde_json::json!({"success": true, "count": 2, "sessions": [sess("a"), sess("b")]}),
        serde_json::json!({"success": true, "count": 2, "sessions": [sess("b"), sess("c")]}),
        serde_json::json!({"success": true, "count": 0, "sessions": []}),
    ];
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    // The polling consumer asks for the server's maximum page size: the
    // request count is sessions / page_size, and the default page is two
    // orders of magnitude smaller than the cap.
    let all = client
        .list_all_sessions(Some(10000), Some("ali"))
        .await
        .expect("both pages must be read");
    let users: Vec<&str> = all.iter().map(|s| s.username.as_str()).collect();
    assert_eq!(users, vec!["a", "b", "c"], "the repeated b collapses exactly once");
    let queries = mock.sessions_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 3, "one request per page, including the empty terminator");
    for (i, q) in queries.iter().enumerate() {
        assert!(q.contains("limit=10000"), "page {i} must carry the page size: {q}");
        assert!(q.contains("keyword=ali"), "page {i} must carry the keyword: {q}");
    }
    assert!(queries[0].contains("offset=0") && queries[1].contains("offset=2"),
        "the offset advances by the rows actually returned: {queries:?}");
}

#[tokio::test]
async fn media_bytes_by_id_fetches_a_single_segment_handle() {
    let mock = Mock::default();
    *mock.media_bytes.lock().unwrap() = Some(b"png-bytes".to_vec());
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let bytes = client
        .media_bytes_by_id("abc123.png")
        .await
        .expect("must fetch the advertised handle");
    assert_eq!(bytes.as_ref(), b"png-bytes");
    assert_eq!(
        mock.media_calls.lock().unwrap().clone(),
        vec!["abc123.png"],
        "one GET, no export side door: this call fetches a handle it was given"
    );
}

#[tokio::test]
async fn ensure_ready_fails_fast_on_a_refusal_state() {
    let mock = Mock::default();
    *mock.register_response.lock().unwrap() = Some(serde_json::json!({
        "success": false, "state": "account_conflict", "occupied_by": "wxid_other",
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let err = client
        .ensure_ready(
            "wxid_mock",
            &serde_json::json!({"wxid": "wxid_mock", "db_path": "X:/db"}),
            Duration::from_millis(700),
        )
        .await
        .expect_err("a refusal must not be waited on");
    match err {
        ClientError::Refused { state, .. } => assert_eq!(state, "account_conflict"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert_eq!(
        *mock.accounts_calls.lock().unwrap(),
        0,
        "a refusal is deterministic: nothing to poll for"
    );
}

// ---- sync_now -----------------------------------------------------------

/// `sync_now` is a **write** (it advances watermarks and may export media), so
/// the contract has two halves: it POSTs with the bearer token and decodes the
/// counters, and nothing else in the client ever triggers it.
#[tokio::test]
async fn sync_now_posts_with_the_bearer_token_and_decodes_counters() {
    let mock = Mock::default();
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let result = client.sync_now().await.expect("decode SyncResult");
    assert!(result.success);
    assert_eq!(result.new_messages, 7);
    assert_eq!(result.revoke_messages, 2);
    assert_eq!(*mock.sync_calls.lock().unwrap(), 1, "exactly one POST /api/v1/sync");
}

/// The other half: the polling paths must not sync on their own. A client that
/// synced while merely asking for readiness would turn every probe into a disk
/// scan of the live database.
#[tokio::test]
async fn no_polling_path_triggers_a_sync() {
    let mock = Mock::default();
    let mut always = mock.clone();
    always.always_indexing = true;
    let base = spawn_mock(always).await;
    let client = Client::new(&base, TOKEN);
    let _ = client.health().await;
    let _ = client
        .ensure_ready(
            "wxid_mock",
            &serde_json::json!({"wxid": "wxid_mock", "db_path": "X:/db"}),
            Duration::from_millis(300),
        )
        .await;
    assert_eq!(
        *mock.sync_calls.lock().unwrap(),
        0,
        "readiness probing must never sync"
    );
}
