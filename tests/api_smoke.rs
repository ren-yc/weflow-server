//! HTTP contract smoke tests (tower oneshot, no network): health, auth,
//! messages, sessions, chatlab pull, contacts, group-members, media, sync.

mod common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tower::ServiceExt;

use weflow_server::db::scan::AccountInfo;
use weflow_server::keystore;
use weflow_server::server::{self, AccountHandle};
use weflow_server::store::Store;
use weflow_server::sync::AccountSync;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn test_state(dir: &std::path::Path) -> Arc<server::AppState> {
    test_state_with(dir, |_, _| {})
}

/// `test_state`, but `mutate(storage, key)` runs against the freshly built
/// fixture before the first sync — used to reshape a database schema.
fn test_state_with(
    dir: &std::path::Path,
    mutate: impl FnOnce(&std::path::Path, &[u8; 32]),
) -> Arc<server::AppState> {
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let key_bytes = key.0;
    let storage = common::build_wechat_account(dir, &key_bytes);
    mutate(&storage, &key_bytes);
    let store = Arc::new(RwLock::new(Store::default()));

    let info = AccountInfo {
        wxid: common::FAKE_WXID.to_string(),
        dir: dir.to_path_buf(),
        db_storage: storage.clone(),
        session_db: Some(storage.join("session/session.db")),
    };
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let cfg = weflow_server::config::Config {
        host: "127.0.0.1".into(),
        port: 5033,
        log: "info".into(),
        watch_debounce_ms: 20,
        watch_fallback_ms: 0,
        media_export_dir: dir.join("api-media"),
        base_url: None,
        show_token: false,
        data_dir: dir.join("data"),
    };
    // State first: the event bus lives there and the sync engine publishes onto
    // it (mirrors `register_account`).
    let state = Arc::new(server::AppState::new(cfg, TOKEN.to_string(), shutdown_tx));

    let sync = Arc::new(Mutex::new(AccountSync::with_channel(
        common::FAKE_WXID,
        &storage,
        weflow_server::keystore::KeyMap::from(key),
        store.clone(),
        state.events.clone(),
    )));
    sync.lock().full_sync().unwrap();

    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(2), // Ready
        error: Mutex::new(None),
        store,
        sync,
        media_keys: None,
        watcher: Mutex::new(None),
    });
    state
        .accounts
        .lock()
        .insert(common::FAKE_WXID.to_string(), handle);
    state
}

fn request(method: &str, uri: &str, token: Option<&str>) -> Request<Body> {
    let mut req = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
    if let Some(t) = token {
        req.headers_mut().insert(header::AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
    }
    req
}

async fn json_body(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn health_is_open() {
    let dir = common::tmp_dir("smoke-health");
    let state = test_state(&dir);
    let app = server::build_router(state);
    for uri in ["/health", "/api/v1/health"] {
        let resp = app.clone().oneshot(request("GET", uri, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

/// The SSE endpoint has no readiness gate (qqflow-server parity): with zero
/// accounts it answers 200 and streams, while *business* endpoints still 503
/// because there is genuinely no index to query yet. Auth is still enforced.
#[tokio::test]
async fn sse_has_no_readiness_gate_but_business_endpoints_do() {
    let dir = common::tmp_dir("smoke-ssegate");
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        weflow_server::config::Config {
            host: "127.0.0.1".into(),
            port: 0,
            log: "info".into(),
            watch_debounce_ms: 10,
            watch_fallback_ms: 0,
            media_export_dir: dir.join("api-media"),
            base_url: None,
            show_token: false,
            data_dir: dir.join("data"),
        },
        TOKEN.to_string(),
        shutdown_tx,
    ));
    assert!(state.accounts.lock().is_empty());
    let app = server::build_router(state);

    // unauthenticated SSE is still rejected
    let resp = app
        .clone()
        .oneshot(request("GET", "/api/v1/push/messages", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // authenticated SSE with zero accounts: 200, not 503
    let resp = app
        .clone()
        .oneshot(request("GET", "/api/v1/push/messages", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "SSE must not gate on account readiness"
    );

    // business endpoints keep their 503 gate (no index to serve)
    let resp = app
        .oneshot(request("GET", "/api/v1/sessions", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn auth_required_on_business_endpoints() {
    let dir = common::tmp_dir("smoke-auth");
    let state = test_state(&dir);
    let app = server::build_router(state);
    let resp = app
        .clone()
        .oneshot(request("GET", "/api/v1/sessions", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // bad token also 401
    let resp = app
        .oneshot(request("GET", "/api/v1/sessions", Some("wrong")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn messages_contract() {
    let dir = common::tmp_dir("smoke-msgs");
    let state = test_state(&dir);
    let app = server::build_router(state);
    let uri = format!(
        "/api/v1/messages?talker={}&limit=10&access_token={}",
        common::FAKE_FRIEND, TOKEN
    );
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert_eq!(body["count"].as_i64().unwrap(), 4);
    let msgs = body["messages"].as_array().unwrap();
    let first = &msgs[0];
    assert!(first["serverId"].is_string(), "serverId must be string");
    assert!(first["localType"].is_number());
    assert!(first["createTime"].is_number());
    assert!(first["content"].is_string());
    assert!(first["rawContent"].is_string());
    assert!(first["parsedContent"].is_string());
    // image message carries media metadata
    let img = msgs.iter().find(|m| m["localType"] == 3).unwrap();
    assert_eq!(img["media"]["fileName"], "aabbccddeeff00112233445566778899.jpg");

    // chatlab=1 shape
    let uri = format!(
        "/api/v1/messages?talker={}&chatlab=1&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["chatlab"]["generator"], "weflow-server");
    assert_eq!(body["meta"]["platform"], "wechat");
    assert!(body["messages"].as_array().unwrap().len() >= 4);
    // group message displays with group type
    assert_eq!(body["meta"]["type"], "group");

    // missing talker -> 400
    let uri = format!("/api/v1/messages?access_token={TOKEN}");
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // unknown conversation -> 404
    let uri = format!("/api/v1/messages?talker=who&access_token={TOKEN}");
    let resp = app.oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_contacts_group_members() {
    let dir = common::tmp_dir("smoke-sess");
    let state = test_state(&dir);
    let app = server::build_router(state);

    let uri = format!("/api/v1/sessions?access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let group = sessions.iter().find(|s| s["username"] == common::FAKE_GROUP).unwrap();
    assert_eq!(group["displayName"], "项目群");
    assert_eq!(group["unreadCount"], 2);

    let uri = format!("/api/v1/contacts?access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 3);
    let friend = body["contacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["username"] == common::FAKE_FRIEND)
        .unwrap();
    assert_eq!(friend["displayName"], "客户张三"); // remark priority

    let uri = format!(
        "/api/v1/group-members?chatroomId={}&includeMessageCounts=1&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "sender universe of the group conversation");
    for m in members {
        assert!(m["messageCount"].as_i64().unwrap() >= 1);
        assert!(m["wxid"].is_string());
    }
}

/// `/api/v1/contacts` pages by `offset` with a deterministic order and reports
/// `total` / `hasMore`, so a client can walk the whole address book instead of
/// silently receiving only the first `limit` rows (the default is 100).
#[tokio::test]
async fn contacts_paginate_by_offset() {
    let dir = common::tmp_dir("smoke-contactpage");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // full page: 3 fixture contacts, nothing more to fetch
    let uri = format!("/api/v1/contacts?access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 3);
    assert_eq!(body["total"].as_i64().unwrap(), 3);
    assert_eq!(body["hasMore"], false);

    // walk it one row at a time and collect the usernames
    let mut seen: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        let uri = format!("/api/v1/contacts?limit=1&offset={offset}&access_token={TOKEN}");
        let (status, body) =
            json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"].as_i64().unwrap(), 3, "total is offset-independent");
        let page = body["contacts"].as_array().unwrap();
        assert_eq!(page.len(), 1, "limit is honoured");
        seen.push(page[0]["username"].as_str().unwrap().to_string());
        if !body["hasMore"].as_bool().unwrap() {
            break;
        }
        offset += 1;
        assert!(offset < 10, "pagination must terminate");
    }
    assert_eq!(seen.len(), 3, "every contact reachable via offset: {seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "no row repeated across pages: {seen:?}");

    // offset past the end: empty page, no phantom hasMore
    let uri = format!("/api/v1/contacts?offset=99&access_token={TOKEN}");
    let (status, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 0);
    assert_eq!(body["hasMore"], false);
}

/// `/api/v1/sessions` pages by `offset` like `/api/v1/contacts`: the fixture
/// holds 2 sessions (group newer than friend), so `limit=1` splits them into
/// two disjoint pages whose union is the whole list; an `offset` past the end
/// returns an empty page (count=0, success=true). `offset` applies AFTER the
/// keyword filter and the stable sort, and to the chatlab shape too.
#[tokio::test]
async fn sessions_paginate_by_offset() {
    let dir = common::tmp_dir("smoke-sesspage");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // walk one row at a time; the two pages must be disjoint and cover all
    let mut seen: Vec<String> = Vec::new();
    for offset in 0..2 {
        let uri = format!("/api/v1/sessions?limit=1&offset={offset}&access_token={TOKEN}");
        let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true);
        assert_eq!(body["count"].as_i64().unwrap(), 1, "limit honoured at offset {offset}");
        seen.push(body["sessions"][0]["username"].as_str().unwrap().to_string());
    }
    assert_eq!(seen.len(), 2, "both fixture sessions reachable via offset: {seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 2, "no session repeated across pages: {seen:?}");
    // newest first: the group (1700000015) precedes the friend (1700000010)
    assert_eq!(seen[0], common::FAKE_GROUP);
    assert_eq!(seen[1], common::FAKE_FRIEND);

    // offset past the end: empty page, success stays true
    let uri = format!("/api/v1/sessions?offset=99&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert_eq!(body["count"].as_i64().unwrap(), 0);
    assert!(body["sessions"].as_array().unwrap().is_empty());

    // chatlab shape pages the same way
    let uri = format!("/api/v1/sessions?limit=1&offset=1&chatlab=1&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let page = body["sessions"].as_array().unwrap();
    assert_eq!(page.len(), 1, "chatlab honours limit+offset");
    assert_eq!(page[0]["id"], common::FAKE_FRIEND, "chatlab page 2 is the friend");
    let uri = format!("/api/v1/sessions?offset=99&chatlab=1&access_token={TOKEN}");
    let (_, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert!(body["sessions"].as_array().unwrap().is_empty(), "chatlab offset past the end is empty");

    // offset is relative to the FILTERED set: keyword first, then the page
    let uri = format!("/api/v1/sessions?keyword=项目群&offset=1&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 0, "offset counts filtered rows only");

    // POST body transport carries offset too (merged params, body wins)
    let body = serde_json::json!({ "limit": 1, "offset": 1, "access_token": TOKEN });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessions"][0]["username"], common::FAKE_FRIEND, "POST body offset honoured");
}

/// Real WeChat 4.x `SessionTable` has no session-name column (probed against
/// a live account: 315 rows, zero matches for every name alias the index
/// looks for). The session list must still emit human names by falling back
/// to contacts instead of leaking the raw wxid, and keyword search by name
/// must keep working.
#[tokio::test]
async fn session_names_fall_back_to_contacts_without_a_name_column() {
    let dir = common::tmp_dir("smoke-noname");
    let state = test_state_with(&dir, |storage, key| {
        common::rewrite_session_db_without_name_column(storage, key);
    });
    let app = server::build_router(state);

    // default shape
    let uri = format!("/api/v1/sessions?access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let group = sessions.iter().find(|s| s["username"] == common::FAKE_GROUP).unwrap();
    assert_eq!(group["displayName"], "项目群", "group name via contact nickname");
    let friend = sessions.iter().find(|s| s["username"] == common::FAKE_FRIEND).unwrap();
    assert_eq!(friend["displayName"], "客户张三", "remark beats nickname");
    // the session row still carries its own data
    assert_eq!(group["unreadCount"], 2);
    assert_eq!(group["summary"], "[图片]");

    // chatlab shape
    let uri = format!("/api/v1/sessions?chatlab=1&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let group = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == common::FAKE_GROUP)
        .unwrap();
    assert_eq!(group["name"], "项目群");

    // keyword search by human name (was a guaranteed 0-hit before)
    let uri = format!("/api/v1/sessions?keyword=项目群&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 1, "name search must hit");
    assert_eq!(body["sessions"][0]["username"], common::FAKE_GROUP);

    // a session with no contact entry keeps the username as the last resort
    let uri = format!("/api/v1/sessions?keyword=nope-nobody&access_token={TOKEN}");
    let (_, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(body["count"].as_i64().unwrap(), 0);
}

#[tokio::test]
async fn chatlab_pull_contract() {
    let dir = common::tmp_dir("smoke-pull");
    let state = test_state(&dir);
    let app = server::build_router(state);
    let uri = format!(
        "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["chatlab"]["version"].is_string());
    assert_eq!(body["meta"]["groupId"], common::FAKE_GROUP);
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    let sync = &body["sync"];
    assert_eq!(sync["hasMore"], false);
    assert!(sync["watermark"].as_i64().unwrap() >= 1_700_000_100);
    let m = &body["messages"][0];
    assert!(m["platformMessageId"].is_string());
    assert!(m["sender"].is_string());
}

/// Paginating with the cursors the server hands back must serve every message
/// exactly once.
///
/// Regression: `nextSince` used to be the newest timestamp in the WHOLE
/// conversation rather than the page's own last timestamp, and `since` was
/// inclusive. Feeding the pair back therefore jumped straight to the end —
/// page 2 came back empty and every message in between was silently dropped.
/// The single-page `chatlab_pull_contract` above cannot see this: it never
/// takes a second page.
#[tokio::test]
async fn chatlab_pull_pagination_drains_every_message() {
    let dir = common::tmp_dir("smoke-pullpage");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // The group fixture holds 4 messages, one per second, so limit=1 forces a
    // page per message (a page always covers a whole second).
    let mut ids: Vec<String> = Vec::new();
    let mut uri = format!(
        "/api/v1/sessions/{}/messages?limit=1&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let mut pages = 0;
    loop {
        let (status, body) =
            json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let page = body["messages"].as_array().unwrap();
        assert_eq!(page.len(), 1, "one message per second-group at limit=1");
        ids.push(page[0]["platformMessageId"].as_str().unwrap().to_string());
        pages += 1;
        assert!(pages <= 4, "must not loop past the 4 fixture messages");
        if !body["sync"]["hasMore"].as_bool().unwrap() {
            assert_eq!(body["sync"]["nextOffset"], 0, "drained cursor resets offset");
            break;
        }
        let since = body["sync"]["nextSince"].as_i64().unwrap();
        let offset = body["sync"]["nextOffset"].as_i64().unwrap();
        uri = format!(
            "/api/v1/sessions/{}/messages?since={since}&offset={offset}&limit=1&access_token={}",
            common::FAKE_GROUP, TOKEN
        );
    }
    assert_eq!(pages, 4, "4 messages at limit=1 means 4 pages");
    assert_eq!(ids.len(), 4);
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), 4, "no message served twice: {ids:?}");

    // `since` is exclusive: resuming from a message's own timestamp must not
    // hand that message back again.
    let uri = format!(
        "/api/v1/sessions/{}/messages?since=1700000100&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (_, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(
        body["messages"].as_array().unwrap().len(),
        3,
        "exclusive since drops the boundary second"
    );
}

#[tokio::test]
async fn media_and_sync_endpoints() {
    let dir = common::tmp_dir("smoke-media");
    let state = test_state(&dir);
    let app = server::build_router(state);
    // media: not exported -> 404 (and traversal attempts -> 400)
    let uri = format!("/api/v1/media/{}/images/x.jpg?access_token={}", common::FAKE_GROUP, TOKEN);
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let uri = format!("/api/v1/media/{}/images/..%2F..%2Fetc%2Fpasswd?access_token={}", common::FAKE_GROUP, TOKEN);
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // manual sync returns counts
    let uri = format!("/api/v1/sync?access_token={TOKEN}");
    let (status, body) = json_body(app.oneshot(request("POST", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["success"] == true);
    assert_eq!(body["newMessages"], 0, "nothing changed -> zero new");
}

#[tokio::test]
async fn accounts_registration_is_idempotent_and_health_lists_state() {
    let dir = common::tmp_dir("smoke-acct");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // Re-registering the already-ready fake account must answer
    // `already_ready` (with real status) instead of rebuilding.
    let body = serde_json::json!({ "wxid": common::FAKE_WXID, "access_token": TOKEN });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/accounts")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert_eq!(body["state"], "already_ready");
    assert_eq!(body["status"], "ready");

    // /health (unauthenticated) now carries the per-account state list.
    let (status, body) = json_body(app.oneshot(request("GET", "/health", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok", "all accounts ready");
    let acc = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["wxid"] == common::FAKE_WXID)
        .expect("fake account listed");
    assert_eq!(acc["state"], "ready");
    assert!(acc["message_count"].as_i64().unwrap() >= 1);
}

/// Direct `register_account` idempotency (the lock-level guard that also
/// covers the concurrent re-registration window): the second call returns
/// the same handle with `is_new == false`, and no watcher/rebuild happens.
#[test]
fn register_account_is_idempotent_at_registry_level() {
    let dir = std::env::temp_dir().join(format!("regacct-{}", std::process::id()));
    let key = weflow_server::keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let key_bytes = key.0;
    let storage = common::build_wechat_account(&dir, &key_bytes);
    let info = AccountInfo {
        wxid: "wxid_fake_reg_acct".into(),
        dir: dir.clone(),
        db_storage: storage.clone(),
        session_db: Some(storage.join("session/session.db")),
    };
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        weflow_server::config::Config {
            host: "127.0.0.1".into(),
            port: 0,
            log: "info".into(),
            watch_debounce_ms: 10,
            watch_fallback_ms: 0,
            media_export_dir: dir.join("api-media"),
            base_url: None,
            show_token: false,
            data_dir: dir.join("data"),
        },
        TOKEN.to_string(),
        shutdown_tx,
    ));

    let keymap = weflow_server::keystore::KeyMap::from(key);
    let (h1, is_new1) =
        server::register_account(&state, info.clone(), keymap.clone(), None);
    assert!(is_new1, "first registration creates the handle");
    assert_eq!(h1.status(), weflow_server::server::AccountStatus::Indexing);

    // second registration (still indexing) -> same handle, no replacement
    let (h2, is_new2) = server::register_account(&state, info.clone(), keymap.clone(), None);
    assert!(!is_new2, "re-registration reuses the live handle");
    assert!(Arc::ptr_eq(&h1, &h2), "same handle object, never rebuilt");

    // once ready, re-registration still reuses (no downgrade to indexing)
    h1.set_status(weflow_server::server::AccountStatus::Ready);
    let (h3, is_new3) = server::register_account(&state, info.clone(), keymap, None);
    assert!(!is_new3, "ready account stays ready on re-registration");
    assert!(Arc::ptr_eq(&h1, &h3));
    let _ = std::fs::remove_dir_all(&dir);
}
