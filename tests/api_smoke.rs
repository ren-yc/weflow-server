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

    let stopped = sync.lock().stop_flag();
    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(2), // Ready
        error: Mutex::new(None),
        store,
        sync,
        media_keys: None,
        watcher: Mutex::new(None),
        stopped,
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
    // Absent source fields flatten to "" rather than null. The fixture's
    // contact table has no avatar column at all, so `avatarUrl` is the
    // None-valued case; `alias` is present and non-empty for this row.
    assert_eq!(friend["avatarUrl"], "", "absent field is an empty string, not null");
    assert_eq!(friend["alias"], "zhangsan001");
    for c in body["contacts"].as_array().unwrap() {
        for field in ["nickname", "remark", "alias", "avatarUrl"] {
            assert!(c[field].is_string(), "{field} is a string, never null: {c}");
        }
    }

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

/// The ChatLab session face must say when it truncated.
///
/// Regression: it returned `{sessions:[...]}` with no `count` and no `page`.
/// A reader that treats a missing `page` block as "this is the complete set"
/// (which is what the pull specification says it means) silently lost every
/// session past `limit` — and the default limit is small enough to hit in a
/// normal account.
#[tokio::test]
async fn chatlab_sessions_page_reports_more() {
    let dir = common::tmp_dir("smoke-sessmore");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // Two sessions in the fixture; take them one at a time.
    let uri = format!("/api/v1/sessions?chatlab=1&limit=1&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 1, "count is the page size");
    assert_eq!(body["page"]["hasMore"], true, "a truncated page must say so");
    let cursor = body["page"]["nextCursor"]
        .as_str()
        .expect("truncation must hand back a cursor")
        .to_string();

    // Following the cursor serves the remainder and then reports completion.
    let uri = format!("/api/v1/sessions?chatlab=1&limit=1&cursor={cursor}&access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"].as_i64().unwrap(), 1);
    assert_eq!(body["sessions"][0]["id"], common::FAKE_FRIEND, "cursor resumes after the page");
    assert_eq!(body["page"]["hasMore"], false);
    assert!(body["page"]["nextCursor"].is_null(), "no cursor once drained");

    // A page that already covers everything reports completion immediately.
    let uri = format!("/api/v1/sessions?chatlab=1&limit=50&access_token={TOKEN}");
    let (_, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(body["count"].as_i64().unwrap(), 2);
    assert_eq!(body["page"]["hasMore"], false);
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

/// ChatLab's `accountName` and `groupNickname` are two different names: the
/// contact's own display name, and their per-chatroom card (群昵称). Serving the
/// contact's 备注 as `groupNickname` made the two identical for anyone with a
/// remark and wrong for everyone else.
#[tokio::test]
async fn chatlab_splits_account_name_from_group_nickname() {
    let dir = common::tmp_dir("smoke-pullnames");
    let state = test_state(&dir);
    // The fixture has no chatroom card table; inject one for a real group
    // sender (wxid_member_b, nickname 李四, no remark).
    {
        let accounts = state.accounts.lock();
        let handle = accounts.get(common::FAKE_WXID).unwrap();
        handle
            .store
            .write()
            .group_cards
            .entry(common::FAKE_GROUP.to_string())
            .or_default()
            .insert("wxid_member_b".to_string(), "四哥".to_string());
    }
    let app = server::build_router(state);

    let uri = format!(
        "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    let member = body["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["platformId"] == "wxid_member_b")
        .expect("wxid_member_b in members");
    assert_eq!(member["accountName"], "李四", "the contact's own name");
    assert_eq!(member["groupNickname"], "四哥", "the chatroom card, not the 备注");

    let msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sender"] == "wxid_member_b")
        .expect("a message from wxid_member_b");
    assert_eq!(msg["accountName"], "李四");
    assert_eq!(msg["groupNickname"], "四哥", "messages carry the card too");

    // A private chat has no cards at all: `groupNickname` is empty rather than
    // a copy of the remark (客户张三 is wxid_friend_a's 备注).
    let uri = format!(
        "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
        common::FAKE_FRIEND, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let m = body["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["platformId"] == common::FAKE_FRIEND)
        .expect("the friend in members");
    assert_eq!(m["accountName"], "客户张三", "remark wins for the display name");
    assert_eq!(m["groupNickname"], "", "a 备注 is not a group nickname");

    // Same split on the chatlab branch of /api/v1/messages.
    let uri = format!(
        "/api/v1/messages?talker={}&chatlab=1&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["sender"] == "wxid_member_b")
        .expect("a message from wxid_member_b");
    assert_eq!(msg["accountName"], "李四");
    assert_eq!(msg["groupNickname"], "四哥");
}

/// `messages[].type` is the canonical ChatLab 0.0.2 code, a different space
/// from the native `localType`: an image is 1 there and 3 natively, and the
/// unassigned code 6 must never appear.
#[tokio::test]
async fn chatlab_type_uses_the_canonical_enum() {
    let dir = common::tmp_dir("smoke-pulltype");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // The friend conversation holds text (1), an image (3) and a revoke
    // sysmsg (10002).
    let uri = format!(
        "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
        common::FAKE_FRIEND, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let pull = body["messages"].as_array().unwrap().clone();

    // Pair each ChatLab type with the native localType of the same serverId.
    let uri = format!(
        "/api/v1/messages?talker={}&limit=5000&access_token={}",
        common::FAKE_FRIEND, TOKEN
    );
    let (_, native) =
        json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    let native = native["messages"].as_array().unwrap().clone();
    let local_of = |server_id: &str| -> i64 {
        native
            .iter()
            .find(|m| m["serverId"] == server_id)
            .and_then(|m| m["localType"].as_i64())
            .unwrap_or_else(|| panic!("no native row for {server_id}"))
    };

    let mut seen = std::collections::BTreeMap::new();
    for m in &pull {
        let t = m["type"].as_i64().unwrap();
        assert_ne!(t, 6, "6 is unassigned in ChatLab 0.0.2: {m:?}");
        seen.insert(local_of(m["platformMessageId"].as_str().unwrap()), t);
    }
    assert_eq!(seen.get(&1), Some(&0), "text 1 -> TEXT 0");
    assert_eq!(seen.get(&3), Some(&1), "image 3 -> IMAGE 1, not 3");
    assert_eq!(
        seen.get(&10002),
        Some(&81),
        "a sysmsg that decoded a revoke -> RECALL 81"
    );
}

/// Every WeChat code the parser recognizes maps into the published ChatLab
/// enum, and the appmsg code (49) resolves by payload rather than collapsing
/// to one type.
#[test]
fn chatlab_type_table_matches_the_published_enum() {
    use weflow_server::server::handlers::chatlab_type;

    let plain = |xml: &str| weflow_server::parser::parse_message(49, 1, 1, xml);
    let none = weflow_server::parser::parse_message(1, 1, 1, "hi");

    // Basic types (0-19); 6 is unassigned.
    assert_eq!(chatlab_type(1, &none), 0); // TEXT
    assert_eq!(chatlab_type(3, &none), 1); // IMAGE
    assert_eq!(chatlab_type(34, &none), 2); // VOICE
    assert_eq!(chatlab_type(43, &none), 3); // VIDEO
    assert_eq!(chatlab_type(47, &none), 5); // EMOJI
    assert_eq!(chatlab_type(48, &none), 8); // LOCATION — was 7 (LINK)
    // Interactive types (20-39).
    assert_eq!(chatlab_type(50, &none), 24); // SHARE — was the unassigned 6
    assert_eq!(chatlab_type(42, &none), 27); // CONTACT — was 99
    // Unknown codes fall through to OTHER.
    assert_eq!(chatlab_type(9999, &none), 99);

    // 49 by payload: a link/card, a file attachment, a quote reply.
    let link = plain("<msg><appmsg><type>5</type><title>某文章</title></appmsg></msg>");
    assert_eq!(chatlab_type(49, &link), 7, "LINK");
    let file = plain("<msg><appmsg><type>6</type><title>报表.xlsx</title></appmsg></msg>");
    assert_eq!(chatlab_type(49, &file), 4, "FILE");
    let reply = plain(
        "<msg><appmsg><type>57</type><title>好的</title>\
         <refermsg><svrid>8100000000000000001</svrid><content>原文</content></refermsg>\
         </appmsg></msg>",
    );
    assert_eq!(chatlab_type(49, &reply), 25, "REPLY");
    // Quoting a FILE stays a REPLY. The refermsg carries the quoted message's
    // own <type>6</type>, so a lookup that is not scope-aware reads the inner 6
    // and downgrades the reply to FILE.
    let reply_to_file = plain(
        "<msg><appmsg><type>57</type><title>收到</title>\
         <refermsg><type>6</type><svrid>8100000000000000002</svrid>\
         <content>报表.xlsx</content></refermsg></appmsg></msg>",
    );
    assert_eq!(chatlab_type(49, &reply_to_file), 25, "REPLY over FILE");

    // 10000/10002 split on whether a revoke payload actually decoded, not on
    // the code: a plain sysmsg is SYSTEM even when it arrives as 10002.
    let revoke = weflow_server::parser::parse_message(
        10002,
        1,
        1,
        "<sysmsg type=\"revokemsg\"><revokemsg><msgid>1</msgid>\
         <replacemsg>对方撤回了一条消息</replacemsg></revokemsg></sysmsg>",
    );
    assert_eq!(chatlab_type(10002, &revoke), 81, "RECALL");
    let notice = weflow_server::parser::parse_message(10002, 1, 1, "<sysmsg>群公告</sysmsg>");
    assert_eq!(chatlab_type(10002, &notice), 80, "SYSTEM");
    assert_eq!(chatlab_type(10000, &notice), 80);
}

/// The mixed face always carries `messages[].replyToMessageId`, `null` when the
/// message quotes nothing, and that shape is frozen: downstream already reads
/// it, so dropping the key would be a breaking change.
///
/// The pull face deliberately differs — it **omits** the key when there is no
/// quote (covered by `pull_carries_reply_to_message_id_only_when_a_quote_exists`).
/// The two faces disagree on purpose; do not "unify" them.
#[tokio::test]
async fn mixed_face_always_carries_reply_to_message_id() {
    let dir = common::tmp_dir("smoke-mixedreply");
    let state = test_state(&dir);
    let app = server::build_router(state);

    let uri = format!(
        "/api/v1/messages?talker={}&chatlab=1&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    for m in body["messages"].as_array().unwrap() {
        assert!(
            m.get("replyToMessageId").is_some(),
            "WeFlow documents it here: {m:?}"
        );
    }
}

/// `mediaPath` is in WeFlow's field list but we cannot fill it with anything
/// meaningful, so neither ChatLab face emits it. A key that is always `""`
/// advertises support that does not exist; media bytes go through the native
/// shape's `media` object instead. Pinned so it is not reintroduced by accident.
#[tokio::test]
async fn neither_chatlab_face_emits_media_path() {
    let dir = common::tmp_dir("smoke-mediapath");
    let state = test_state(&dir);
    let app = server::build_router(state);

    for uri in [
        format!(
            "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
            common::FAKE_GROUP, TOKEN
        ),
        format!(
            "/api/v1/messages?talker={}&chatlab=1&access_token={}",
            common::FAKE_GROUP, TOKEN
        ),
    ] {
        let (status, body) =
            json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let msgs = body["messages"].as_array().unwrap();
        assert!(!msgs.is_empty(), "fixture must produce messages: {uri}");
        for m in msgs {
            assert!(m.get("mediaPath").is_none(), "{uri}: {m:?}");
        }
    }
}

/// `end=YYYYMMDD` is an INCLUSIVE upper bound, so it must cover the whole day
/// rather than stopping at midnight. The lower bound keeps start-of-day.
#[tokio::test]
async fn pull_end_date_covers_the_whole_day() {
    let dir = common::tmp_dir("smoke-pullend");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // The group fixture sits at 1700000100..1700000103 = 2023-11-14 UTC.
    let uri = format!(
        "/api/v1/sessions/{}/messages?end=20231114&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["messages"].as_array().unwrap().len(),
        4,
        "end-of-day bound keeps that day's messages"
    );
    // The day before excludes them all, so the bound is still a real filter.
    let uri = format!(
        "/api/v1/sessions/{}/messages?end=20231113&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (_, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert!(body["messages"].as_array().unwrap().is_empty());
}

/// The mixed face resolves `end=YYYYMMDD` the same way the pull face does:
/// through the END of that day.
///
/// Regression: this endpoint used to resolve a bare date to the START of the
/// day, so `end=20231114` silently dropped everything sent on the 14th — while
/// the pull face of the same service kept it. One parameter, two meanings,
/// depending on which endpoint you happened to call.
#[tokio::test]
async fn messages_end_date_covers_the_whole_day() {
    let dir = common::tmp_dir("smoke-msgend");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // The group fixture sits at 1700000100..1700000103 = 2023-11-14 UTC.
    let uri = format!(
        "/api/v1/messages?talker={}&end=20231114&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["count"].as_i64().unwrap(),
        4,
        "end-of-day bound keeps that day's messages"
    );

    // The day before still excludes them all, so the bound remains a real filter.
    let uri = format!(
        "/api/v1/messages?talker={}&end=20231113&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (_, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(body["count"].as_i64().unwrap(), 0);
}

/// `start=YYYYMMDD` keeps start-of-day: it is an inclusive LOWER bound, so
/// midnight is the correct edge. Only the upper bound needed changing.
#[tokio::test]
async fn messages_start_date_starts_at_midnight() {
    let dir = common::tmp_dir("smoke-msgstart");
    let state = test_state(&dir);
    let app = server::build_router(state);

    let uri = format!(
        "/api/v1/messages?talker={}&start=20231114&end=20231114&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["count"].as_i64().unwrap(),
        4,
        "a single-day window must contain that day's messages"
    );

    // Starting the day after drops them all: the lower bound is real too.
    let uri = format!(
        "/api/v1/messages?talker={}&start=20231115&limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (_, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(body["count"].as_i64().unwrap(), 0);
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
    // media: not exported -> 404 in the standard envelope. The shape matters:
    // this path (canonicalize failed) used to answer with a bare
    // `{"error": ...}` while the open-failed path a few lines deeper in the
    // handler used the envelope, so one endpoint had two 404 bodies.
    let uri = format!("/api/v1/media/{}/images/x.jpg?access_token={}", common::FAKE_GROUP, TOKEN);
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], 404);
    assert!(body["message"].is_string(), "envelope carries a message: {body}");
    assert!(body.get("error").is_none(), "no bare error key: {body}");

    // traversal attempts -> 400
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

/// `GET /api/v1/media/{id}` serves an exported file by name alone.
///
/// The three-segment route makes the caller repeat the conversation and the
/// media type, which it already got from the message; this one takes only the
/// file name. Resolution happens under the export root, so a caller can never
/// name a path outside it.
/// The pull face carries `replyToMessageId` when — and only when — the message
/// quotes another one.
///
/// It is **omitted** rather than sent as `null`: the standard lists it as an
/// optional *string*, so a `null` hands a reader that trusts the type a value
/// it cannot parse. The same endpoint used to carry no such key at all, which
/// left ChatLab unable to draw a quote even though the parser had the id.
#[tokio::test]
async fn pull_carries_reply_to_message_id_only_when_a_quote_exists() {
    let dir = common::tmp_dir("smoke-quotereply");
    let quoted = 8_200_000_000_000_000_000i64;
    let state = test_state_with(&dir, |storage, key| {
        common::append_group_reply(storage, key, quoted);
    });
    let app = server::build_router(state);

    let uri = format!(
        "/api/v1/sessions/{}/messages?limit=5000&access_token={}",
        common::FAKE_GROUP, TOKEN
    );
    let (status, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let msgs = body["messages"].as_array().unwrap();
    let quoted_id = quoted.to_string();

    // The reference resolves inside the same conversation — a dead id would be
    // worse than no id, because a client cannot tell the difference.
    assert!(
        msgs.iter().any(|m| m["platformMessageId"] == quoted_id.as_str()),
        "the quoted message is on the page: {msgs:?}"
    );

    let reply = msgs
        .iter()
        .find(|m| m["platformMessageId"] == "8200000000000000099")
        .expect("the reply row is served");
    assert_eq!(reply["replyToMessageId"], quoted_id, "the quote points at the parent");

    // A message that quotes nothing omits the key entirely.
    let plain = msgs
        .iter()
        .find(|m| m["platformMessageId"] == "8200000000000000000")
        .expect("a plain message is served");
    assert!(
        plain.get("replyToMessageId").is_none(),
        "no quote -> key omitted, never null: {plain:?}"
    );
}

#[tokio::test]
async fn media_by_id_serves_an_exported_file() {
    let dir = common::tmp_dir("smoke-mediaid");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // Lay out an exported image exactly the way the exporter writes it.
    let talker_dir = dir.join("api-media").join(common::FAKE_GROUP).join("images");
    std::fs::create_dir_all(&talker_dir).unwrap();
    let name = "aabbccddeeff00112233445566778899.jpg";
    let bytes: &[u8] = b"\xFF\xD8 fake jpeg \xFF\xD9";
    std::fs::write(talker_dir.join(name), bytes).unwrap();

    let uri = format!("/api/v1/media/{name}?access_token={TOKEN}");
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg",
        "content type comes from the file extension"
    );
    let served = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&served[..], bytes, "the bytes are served verbatim");

    // The three-segment route is untouched by the new one: they differ only in
    // segment count, and both must keep resolving.
    let uri = format!("/api/v1/media/{}/images/{name}?access_token={TOKEN}", common::FAKE_GROUP);
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "three-segment route still works");

    // An unknown name is a 404 in the standard envelope, not a bare error key.
    let uri = format!("/api/v1/media/nosuchfile.jpg?access_token={TOKEN}");
    let (status, body) = json_body(app.clone().oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], 404);
    assert!(body.get("error").is_none(), "no bare error key: {body}");

    // Traversal is rejected by the same shared rule the other media routes use.
    let uri = format!("/api/v1/media/..%2F..%2Fetc%2Fpasswd?access_token={TOKEN}");
    let resp = app.oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Every error the client can provoke carries the same envelope — including the
/// two axum answers by default with an empty body.
///
/// A client that always parses `{success,code,message}` would otherwise have to
/// special-case exactly those two, and a missed exception surfaces as
/// "the server returned something unparseable" rather than as the real cause.
#[tokio::test]
async fn boundary_errors_carry_the_envelope() {
    let dir = common::tmp_dir("smoke-boundary");
    let state = test_state(&dir);
    let app = server::build_router(state);

    // Unknown path: 404, not an empty body.
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", "/api/v1/nope", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], 404);
    assert!(body["message"].is_string(), "envelope carries a message: {body}");

    // Known path, wrong method: 405 — a different status, the same shape.
    let (status, body) =
        json_body(app.oneshot(request("DELETE", "/api/v1/health", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], 405);
    assert!(body["message"].is_string(), "envelope carries a message: {body}");
}

#[tokio::test]
async fn accounts_registration_is_idempotent_and_health_reports_a_scalar_phase() {
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

    // /health is unauthenticated, so it carries a scalar phase and NOTHING
    // that identifies the account or says how many exist on this machine.
    let (status, body) =
        json_body(app.clone().oneshot(request("GET", "/health", None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["account"], "ready");
    assert!(body.get("accounts").is_none(), "/health must not enumerate accounts");
    assert!(
        !body.to_string().contains(common::FAKE_WXID),
        "/health must not leak an account identity"
    );

    // The detail lives behind the token instead.
    let resp = app.clone().oneshot(request("GET", "/api/v1/accounts", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "detail requires the token");
    let uri = format!("/api/v1/accounts?access_token={TOKEN}");
    let (status, body) = json_body(app.oneshot(request("GET", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    let acc = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["wxid"] == common::FAKE_WXID)
        .expect("fake account listed");
    assert_eq!(acc["state"], "ready");
    assert!(acc["message_count"].as_i64().unwrap() >= 1);
    assert!(!acc["db_storage"].as_str().unwrap().is_empty());
}

/// Registering a SECOND wxid is rejected with `account_conflict` at HTTP 200,
/// and the incumbent keeps serving. Deregistering it frees the binding.
#[tokio::test]
async fn a_second_account_is_rejected_until_the_first_is_deregistered() {
    let dir = common::tmp_dir("smoke-conflict");
    let state = test_state(&dir);
    let app = server::build_router(state.clone());

    let post_account = |wxid: &str| {
        let body = serde_json::json!({ "wxid": wxid, "access_token": TOKEN });
        Request::builder()
            .method("POST")
            .uri("/api/v1/accounts")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // The conflict is reported BEFORE the key/path are validated, so a bogus
    // db_path and no key at all still answer `account_conflict` rather than
    // telling the caller anything about key validity.
    let (status, body) =
        json_body(app.clone().oneshot(post_account("wxid_other")).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK, "business rejection, not a 4xx");
    assert_eq!(body["state"], "account_conflict");
    assert_eq!(body["occupied_by"], common::FAKE_WXID);
    assert_eq!(body["occupied_status"], "ready");
    assert_eq!(state.accounts.lock().len(), 1, "the incumbent is untouched");

    // Wrong wxid on the DELETE trips the interlock instead of unbinding.
    let uri = format!("/api/v1/accounts/wxid_other?access_token={TOKEN}");
    let (status, body) =
        json_body(app.clone().oneshot(request("DELETE", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "wxid_mismatch");
    assert_eq!(body["occupied_by"], common::FAKE_WXID);
    assert_eq!(state.accounts.lock().len(), 1);

    // Deregister for real, then the other account can bind.
    let uri = format!("/api/v1/accounts/{}?access_token={TOKEN}", common::FAKE_WXID);
    let (status, body) =
        json_body(app.clone().oneshot(request("DELETE", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "deregistered");
    assert_eq!(body["previous_status"], "ready");
    assert_eq!(body["index_cleared"], true);
    assert_eq!(body["purged_media"], false, "purge_media defaults to false");
    assert_eq!(body["purged_dirs"], 0);

    // Unregistered again: scalar health flips back and business endpoints 503.
    let (_, body) =
        json_body(app.clone().oneshot(request("GET", "/health", None)).await.unwrap()).await;
    assert_eq!(body["status"], "starting");
    assert_eq!(body["account"], "unregistered");
    let uri = format!("/api/v1/sessions?access_token={TOKEN}");
    let resp = app.clone().oneshot(request("GET", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Idempotent: retrying the deregistration is not an error.
    let uri = format!("/api/v1/accounts/{}?access_token={TOKEN}", common::FAKE_WXID);
    let (status, body) =
        json_body(app.oneshot(request("DELETE", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "not_registered");
}

/// Deregistration requires the token, and the POST alias behaves like DELETE.
#[tokio::test]
async fn deregistration_is_authenticated_and_has_a_post_alias() {
    let dir = common::tmp_dir("smoke-dereg-auth");
    let state = test_state(&dir);
    let app = server::build_router(state.clone());

    let uri = format!("/api/v1/accounts/{}", common::FAKE_WXID);
    let resp = app.clone().oneshot(request("DELETE", &uri, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(state.accounts.lock().len(), 1, "an unauthenticated call changes nothing");

    let uri = format!("/api/v1/accounts/{}/deregister?access_token={TOKEN}", common::FAKE_WXID);
    let (status, body) = json_body(app.oneshot(request("POST", &uri, None)).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "deregistered");
    assert!(state.accounts.lock().is_empty());
}

/// Direct `register_account` idempotency (the lock-level guard that also
/// covers the concurrent re-registration window): the second call returns
/// the live handle as `Existing`, and no watcher/rebuild happens.
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
    let h1 = match server::register_account(&state, info.clone(), keymap.clone(), None) {
        server::BindOutcome::Bound(h) => h,
        _ => panic!("first registration must claim the binding"),
    };
    assert_eq!(h1.status(), weflow_server::server::AccountStatus::Indexing);

    // second registration (still indexing) -> same handle, no replacement
    match server::register_account(&state, info.clone(), keymap.clone(), None) {
        server::BindOutcome::Existing(h2) => {
            assert!(Arc::ptr_eq(&h1, &h2), "same handle object, never rebuilt")
        }
        _ => panic!("re-registration must reuse the live handle"),
    }

    // once ready, re-registration still reuses (no downgrade to indexing)
    h1.set_status(weflow_server::server::AccountStatus::Ready);
    match server::register_account(&state, info.clone(), keymap, None) {
        server::BindOutcome::Existing(h3) => assert!(Arc::ptr_eq(&h1, &h3)),
        _ => panic!("ready account must stay ready on re-registration"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
/// Golden 快照：每个 JSON 响应的**逐字节**护栏。
///
/// 为什么必须在 DTO 化**之前**有它：DTO 是重写每一个响应构造，而「断言几个字段」的测试
/// 看不出「某个键消失了」「某个键从 `null` 变成被省略」「某个值的类型变了」——这三类恰恰
/// 是契约明令禁止的改动。快照把整个响应（**含状态码**）固定下来，差异只能靠**改快照**通过，
/// 而改快照会在 review 里显形。
///
/// 易变值（时间戳、夹具临时路径）**掩盖值而不是删键**：删键会把「形状」一起丢掉，
/// 而这个测试存在的意义正是守住形状。
///
/// 更新方式：设 `UPDATE_GOLDEN=1` 后跑本测试，然后**人工读一遍 diff**——
/// 自动生成的快照等于没有快照。
mod golden {
    use super::*;

    /// 值易变、但键必须留下的字段。按**键名**匹配：同一个字段换个端点仍叫这个名字，
    /// 所以清单不随端点增长。
    const VOLATILE_KEYS: &[&str] = &[
        // 每次请求都不同
        "exportedAt",
        // 夹具的临时目录：每次运行都不同
        "db_storage",
        "dbPath",
        "dir",
        "dataDir",
        "sessionDb",
        "exportPath",
        "localPath",
        // 由 `chrono::Utc::now()` 派生：每次运行都不同
        "watermark",
        "nextSince",
    ];

    fn golden_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
    }

    /// 把易变值换成占位符，并断言夹具路径**没有从别处漏出去**。
    ///
    /// 那条断言是这套快照的关键：漏掩盖一个易变字段时，测试会**响亮地失败**，
    /// 而不是每次运行都产生一份新快照——后者会让人习惯性地点「更新快照」，
    /// 护栏于是名存实亡。
    fn mask(value: &mut serde_json::Value, tmp: &str) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map.iter_mut() {
                    if VOLATILE_KEYS.contains(&key.as_str()) {
                        *val = serde_json::Value::String("<volatile>".into());
                    } else {
                        mask(val, tmp);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for it in items.iter_mut() {
                    mask(it, tmp);
                }
            }
            serde_json::Value::String(s) => {
                assert!(!s.contains(tmp), "夹具路径从掩码外漏出：{s}");
            }
            _ => {}
        }
    }

    /// **时钟哨兵**：快照里不得出现接近「现在」的时间戳。
    ///
    /// 这一条决定这套护栏能不能活下去。夹具的数据停在固定的历史时刻，所以任何接近
    /// 现在的时间戳都必然来自 `Utc::now()`——它每次运行都会变，于是快照要么天天漂移，
    /// 要么被人习惯性地点「更新快照」，两种结局都是护栏失效。
    ///
    /// 第一版只断言「夹具路径没漏出去」，因此**漏掉了 `sync.watermark`**：路径是对的，
    /// 时间是变的。按名字枚举易变字段终究会漏，把「时间」这一整类圈出来才兜得住。
    fn assert_no_wall_clock(value: &serde_json::Value, name: &str) {
        const ONE_YEAR: i64 = 365 * 86_400;
        let now = chrono::Utc::now().timestamp();
        match value {
            serde_json::Value::Number(n) => {
                if let Some(v) = n.as_i64()
                    // **不假设单位**：秒与毫秒各比一次。第一版只写了秒，于是
                    // `updatedAt: 1790426034799`（毫秒）从哨兵底下漏了过去——
                    // 一个用来防「时间类字段漏掩码」的哨兵，自己带着单位假设。
                    && ((v - now).abs() < ONE_YEAR || (v - now * 1000).abs() < ONE_YEAR * 1000)
                {
                    panic!(
                        "{name}：快照里出现接近「现在」的时间戳 {v}（now={now}）——它每次运行都会变，必须加进 VOLATILE_KEYS"
                    );
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values() {
                    assert_no_wall_clock(v, name);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    assert_no_wall_clock(v, name);
                }
            }
            _ => {}
        }
    }

/// 原始响应里的键**按出现顺序**（含嵌套）。
    ///
    /// 为什么单独记它：上面那份 body 是**解析成 `Value` 再序列化**的，而 `serde_json::Map`
    /// 默认是 BTreeMap —— **键会被排序**，于是原始顺序在比较里丢掉了。把顺序单独钉住，
    /// 「换 DTO 时顺手改了键序」这类无意义但真实的改动才会显形。
    ///
    /// 实现是扫 `"..."` 后紧跟冒号的位置，够用且不引依赖：值里的中文冒号不会误判，
    /// 而真正的风险（重排、删键、加键）都能看见。
    fn key_order(raw: &str) -> Vec<String> {
        let bytes = raw.as_bytes();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut j = start;
            let mut esc = false;
            while j < bytes.len() {
                if esc {
                    esc = false;
                } else if bytes[j] == b'\\' {
                    esc = true;
                } else if bytes[j] == b'"' {
                    break;
                }
                j += 1;
            }
            let mut k = j + 1;
            while k < bytes.len() && (bytes[k] as char).is_whitespace() {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b':' {
                out.push(raw[start..j].to_string());
            }
            i = j + 1;
        }
        out
    }

    /// 端点清单：名字 → (方法, URI, **可选 JSON body**)。名字同时是快照文件名。
    ///
    /// **顺序是有意的**：会改状态的那几个（注销）排在最后，否则它们会让后面端点的
    /// 快照取决于前面跑过什么。
    fn endpoints() -> Vec<(&'static str, &'static str, String, Option<serde_json::Value>)> {
        let g = common::FAKE_GROUP;
        let wxid = common::FAKE_WXID;
        vec![
            ("health", "GET", "/health".to_string(), None),
            ("accounts", "GET", format!("/api/v1/accounts?access_token={TOKEN}"), None),
            ("sessions-native", "GET", format!("/api/v1/sessions?access_token={TOKEN}"), None),
            ("sessions-chatlab", "GET", format!("/api/v1/sessions?chatlab=1&access_token={TOKEN}"), None),
            ("messages-native", "GET", format!("/api/v1/messages?talker={g}&limit=50&access_token={TOKEN}"), None),
            ("messages-chatlab", "GET", format!("/api/v1/messages?talker={g}&limit=50&chatlab=1&access_token={TOKEN}"), None),
            ("messages-media", "GET", format!("/api/v1/messages?talker={g}&limit=50&media=1&access_token={TOKEN}"), None),
            ("pull", "GET", format!("/api/v1/sessions/{g}/messages?limit=50&access_token={TOKEN}"), None),
            ("contacts", "GET", format!("/api/v1/contacts?access_token={TOKEN}"), None),
            (
                "group-members",
                "GET",
                format!("/api/v1/group-members?chatroomId={g}&includeMessageCounts=1&access_token={TOKEN}"),
                None,
            ),
            ("sync", "POST", format!("/api/v1/sync?access_token={TOKEN}"), None),
            // ---- 别名路由与错误信封 ----
            // 错误信封是刚建立的契约，DTO 化最容易在「构造响应的那条路径之外」把它碰坏。
            ("health-alias", "GET", "/api/v1/health".to_string(), None),
            ("error-unauthorized", "GET", "/api/v1/sessions".to_string(), None),
            ("error-unknown-path", "GET", "/api/v1/nope".to_string(), None),
            ("error-method-not-allowed", "DELETE", "/api/v1/health".to_string(), None),
            // ---- 账号面的多形状返回（注册/注销各 3 种 state 的键集不同）----
            (
                // 这条路由**只接受 DELETE**，GET 会得到 405 —— 名字如实反映它记的是什么。
                // （账号面的「详情」形状其实在列表里：`AccountStateView`。）
                "error-accounts-wrong-method",
                "GET",
                format!("/api/v1/accounts/{wxid}?access_token={TOKEN}"),
                None,
            ),
            (
                "accounts-conflict",
                "POST",
                format!("/api/v1/accounts?access_token={TOKEN}"),
                Some(serde_json::json!({ "wxid": wxid, "key": common::FAKE_KEY_HEX })),
            ),
            // ---- SNS（DTO 豁免，但快照很便宜）----
            ("sns-timeline", "GET", format!("/api/v1/sns/timeline?access_token={TOKEN}"), None),
            ("sns-usernames", "GET", format!("/api/v1/sns/usernames?access_token={TOKEN}"), None),
            ("sns-stats", "GET", format!("/api/v1/sns/stats?access_token={TOKEN}"), None),
            // ---- 会改状态的排最后 ----
            (
                "accounts-deregister",
                "POST",
                format!("/api/v1/accounts/{wxid}/deregister?access_token={TOKEN}"),
                None,
            ),
        ]
    }

    #[tokio::test]
    async fn responses_match_their_golden_snapshots() {
        let dir = common::tmp_dir("golden");
        let tmp = dir.to_string_lossy().to_string();
        let state = test_state(&dir);
        let app = server::build_router(state);
        let update = std::env::var("UPDATE_GOLDEN").is_ok();
        std::fs::create_dir_all(golden_dir()).unwrap();

        let mut drifted: Vec<String> = Vec::new();
        for (name, method, uri, payload) in endpoints() {
            let mut req = Request::builder().method(method).uri(&uri);
            let body = match payload {
                Some(v) => {
                    req = req.header("content-type", "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
            let raw_body = String::from_utf8_lossy(&bytes).into_owned();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!("{name} 的响应不是 JSON（HTTP {status}）：{e}")
            });
            // 状态码也进快照：只钉住 body 会漏掉「同一个 body 换了状态码」。
            let keys = key_order(&raw_body);
            let mut snapshot = serde_json::json!({
                "status": status.as_u16(),
                "keys": keys,
                "body": body,
            });
            mask(&mut snapshot, &tmp);
            assert_no_wall_clock(&snapshot, name);
            let actual = serde_json::to_string_pretty(&snapshot).unwrap() + "\n";

            let path = golden_dir().join(format!("{name}.json"));
            if update || !path.exists() {
                std::fs::write(&path, &actual).unwrap();
                println!("[GOLDEN] 写入 {}", path.display());
                continue;
            }
            if std::fs::read_to_string(&path).unwrap() != actual {
                drifted.push(name.to_string());
            }
        }
        assert!(
            drifted.is_empty(),
            "这些端点的响应与快照不一致：{drifted:?}\n\n若改动是有意的，设 UPDATE_GOLDEN=1 重跑后**人工读一遍 diff**。"
        );
    }
}