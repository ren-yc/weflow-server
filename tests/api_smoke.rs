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
    let sync = Arc::new(Mutex::new(AccountSync::new(
        common::FAKE_WXID,
        &storage,
        weflow_server::keystore::KeyMap::from(key),
        store.clone(),
    )));
    sync.lock().full_sync().unwrap();

    let info = AccountInfo {
        wxid: common::FAKE_WXID.to_string(),
        dir: dir.to_path_buf(),
        db_storage: storage.clone(),
        session_db: Some(storage.join("session/session.db")),
    };
    let events = sync.lock().events.clone();
    let handle = Arc::new(AccountHandle {
        info,
        status: AtomicU8::new(2), // Ready
        error: Mutex::new(None),
        store,
        events,
        sync,
        media_keys: None,
        history: Arc::new(Mutex::new(weflow_server::server::HistoryBuf::default())),
        watcher: Mutex::new(None),
    });
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
    
    Arc::new(server::AppState {
        cfg,
        token: TOKEN.to_string(),
        accounts: parking_lot::Mutex::new(
            [(common::FAKE_WXID.to_string(), handle)].into_iter().collect(),
        ),
        shutdown: shutdown_tx,
    })
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
    let state = Arc::new(server::AppState {
        cfg: weflow_server::config::Config {
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
        token: TOKEN.to_string(),
        accounts: parking_lot::Mutex::new(Default::default()),
        shutdown: shutdown_tx,
    });

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