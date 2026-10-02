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
use weflow_client::client::{Client, ClientError, ServerEvent};

const TOKEN: &str = "test-token-0123456789abcdef";

#[derive(Clone, Default)]
struct Mock {
    states: Arc<StdMutex<Vec<String>>>,
    pull_pages: Arc<StdMutex<Vec<serde_json::Value>>>,
    pull_queries: Arc<StdMutex<Vec<String>>>,
    media_calls: Arc<StdMutex<Vec<String>>>,
    media_bytes: Arc<StdMutex<Option<Vec<u8>>>>,
    chatlab_page: Arc<StdMutex<Option<serde_json::Value>>>,
    sse_frames: Arc<StdMutex<Vec<String>>>,
    sse_event: Arc<StdMutex<Option<serde_json::Value>>>,
    sse_reconnect_ids: Arc<StdMutex<Vec<Option<String>>>>,
    messages_query: Arc<StdMutex<Option<String>>>,
    /// When set, the accounts route answers `indexing` forever (timeout test).
    always_indexing: bool,
}

fn parse_query(q: &Option<String>) -> Vec<(String, String)> {
    q.as_deref()
        .unwrap_or("")
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('?').unwrap_or(kv.split_once('=').unwrap_or((kv, "")));
            Some((k.to_string(), v.to_string()))
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
    let _ = mock;
    Json(serde_json::json!({"success": true, "state": "indexing"})).into_response()
}

fn assert_bearer(headers: &HeaderMap) {
    let got = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert_eq!(got, format!("Bearer {TOKEN}"), "auth must go in the Authorization header");
}

async fn accounts_get(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    assert_bearer(&headers);
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
    *mock.messages_query.lock().unwrap() = query;
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
    *mock.messages_query.lock().unwrap() = query;
    Json(serde_json::json!({
        "success": true, "count": 0, "has_more": false,
        "talker": "", "media": {}, "messages": [],
    }))
    .into_response()
}

async fn sse_route(
    State(mock): State<Mock>,
    headers: HeaderMap,
) -> Response {
    assert_bearer(&headers);
    let last_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).map(String::from);
    mock.sse_reconnect_ids.lock().unwrap().push(last_id);
    let frames = mock.sse_frames.lock().unwrap().join("\n");
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

// ---- media_bytes ---------------------------------------------------------

#[tokio::test]
async fn media_bytes_exports_then_retries_once_after_404() {
    let mock = Mock::default();
    let talker = "wxid_mock";
    // First media GET misses (not exported yet); the export side door then
    // registers the bytes so the retry hits.
    *mock.chatlab_page.lock().unwrap() = Some(serde_json::json!({
        "chatlab": {"version": "1", "generator": "mock", "exportedAt": 1},
        "count": 0, "members": [], "messages": [],
        "meta": {"groupId": "", "name": "", "ownerId": "", "platform": "weflow", "type": "chat"},
        "page": {"hasMore": false, "nextCursor": null},
        "talker": talker,
    }));
    let base = spawn_mock(mock.clone()).await;
    let client = Client::new(&base, TOKEN);
    let message: weflow_client::generated::r#gen::types::ChatlabMessage =
        serde_json::from_value(mock_message_json(talker, "abc123.png"))
            .unwrap();
    let bytes = client.media_bytes(&message).await.expect("retry must succeed");
    assert_eq!(bytes.as_ref(), b"png-bytes");
    let calls = mock.media_calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["abc123.png", "abc123.png"], "two GETs: miss then retry");
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
