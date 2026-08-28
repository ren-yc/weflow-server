//! SSE Last-Event-ID replay integration test: real HTTP server + two SSE
//! clients, verifying replay (1000/10min buffer) and incremental ids.

mod common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use weflow_server::db::scan::AccountInfo;
use weflow_server::keystore;
use weflow_server::server::{self, AccountHandle};
use weflow_server::store::Store;
use weflow_server::sync::AccountSync;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct TestServer {
    addr: String,
    #[allow(dead_code)] // held so the account outlives the server in `start`
    handle: Option<Arc<AccountHandle>>,
    state: Arc<server::AppState>,
    /// Built fixture's `db_storage`, so a test can register the account later.
    storage: std::path::PathBuf,
}

/// Serve `state` on an ephemeral port.
async fn serve(state: Arc<server::AppState>) -> String {
    let app: Router = server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr.to_string()
}

/// State + built fixture, with **no account registered** — the cold-start shape.
fn bare_state(dir: &std::path::Path) -> (Arc<server::AppState>, std::path::PathBuf) {
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(dir, &key.0);
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let cfg = weflow_server::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        log: "info".into(),
        watch_debounce_ms: 10,
        watch_fallback_ms: 0,
        media_export_dir: dir.join("api-media"),
        base_url: None,
        show_token: false,
        data_dir: dir.join("data"),
    };
    let state = Arc::new(server::AppState::new(cfg, TOKEN.to_string(), shutdown_tx));
    (state, storage)
}

/// Server with a ready account registered.
async fn start(dir: &std::path::Path) -> TestServer {
    // State first: the event bus now lives here, and the account's sync engine
    // publishes onto it (mirrors `register_account`).
    let (state, storage) = bare_state(dir);
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let store = Arc::new(RwLock::new(Store::default()));

    let sync = Arc::new(Mutex::new(AccountSync::with_channel(
        common::FAKE_WXID,
        &storage,
        keystore::KeyMap::from(key),
        store.clone(),
        state.events.clone(),
    )));
    sync.lock().full_sync().unwrap();

    let info = AccountInfo {
        wxid: common::FAKE_WXID.to_string(),
        dir: dir.to_path_buf(),
        db_storage: storage.clone(),
        session_db: Some(storage.join("session/session.db")),
    };
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
        .insert(common::FAKE_WXID.to_string(), handle.clone());
    let addr = serve(state.clone()).await;
    TestServer { addr, handle: Some(handle), state, storage }
}

/// Server with the fixture built but **no account registered** (cold start).
async fn start_without_account(dir: &std::path::Path) -> TestServer {
    let (state, storage) = bare_state(dir);
    let addr = serve(state.clone()).await;
    TestServer { addr, handle: None, state, storage }
}

/// One SSE connection; returns parsed frames (id, event, data).
async fn sse_frames(
    server: &TestServer,
    last_event_id: Option<u64>,
    window: Duration,
    expected_new: usize,
) -> Vec<(u64, String, String)> {
    let mut req = format!(
        "GET /api/v1/push/messages?access_token={TOKEN} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
        server.addr
    );
    if let Some(id) = last_event_id {
        req.push_str(&format!("Last-Event-ID: {id}\r\n"));
    }
    req.push_str("\r\n");
    let mut stream = TcpStream::connect(&server.addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(stream);
    // consume HTTP head
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
            break;
        }
    }
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    let mut cur_id = 0u64;
    let mut cur_ev = String::new();
    let mut cur_data = String::new();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(deadline - tokio::time::Instant::now(), reader.read_line(&mut line))
            .await;
        match n {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => {
                if !cur_ev.is_empty() {
                    frames.push((cur_id, cur_ev.clone(), cur_data.clone()));
                }
                break;
            }
            Ok(Ok(_)) => {
                let l = line.trim_end();
                if l.starts_with("id:") {
                    cur_id = l[3..].trim().parse().unwrap_or(0);
                } else if l.starts_with("event:") {
                    cur_ev = l[7..].trim().to_string();
                } else if l.starts_with("data:") {
                    cur_data.push_str(&l[5..].trim());
                } else if l.is_empty() {
                    if !cur_ev.is_empty() {
                        frames.push((cur_id, cur_ev.clone(), cur_data.clone()));
                    }
                    cur_id = 0;
                    cur_ev.clear();
                    cur_data.clear();
                    let news = frames.iter().filter(|(_, e, _)| e == "message.new").count();
                    if news >= expected_new {
                        break;
                    }
                }
            }
        }
    }
    frames
}

#[tokio::test]
async fn sse_replay_after_reconnect() {
    let dir = common::tmp_dir("ssereplay");
    let server = start(&dir).await;

    // first connection: no Last-Event-ID (short window, ready only)
    let f1 = sse_frames(&server, None, Duration::from_secs(1), 0).await;
    assert!(f1.iter().any(|(_, e, _)| e == "ready"));

    // connect a second client first, THEN broadcast two live events while it
    // is reading (broadcast does not replay pre-subscription history)
    let event1 = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "111".into(),
        source_name: "a".into(),
        group_name: Some("g".into()),
        content: "hello".into(),
        timestamp: 1700000001,
        media: None,
    });
    let event2 = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "222".into(),
        source_name: "b".into(),
        group_name: Some("g".into()),
        content: "world".into(),
        timestamp: 1700000002,
        media: None,
    });
    let reader = sse_frames(&server, None, Duration::from_secs(8), 2);
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.state.events.send(event1).ok();
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.state.events.send(event2).ok();
    };
    let f2 = tokio::join!(reader, sender).0;
    let new_events: Vec<_> = f2.iter().filter(|(_, e, _)| e == "message.new").collect();
    assert_eq!(new_events.len(), 2, "two live events: {f2:?}");
    let id1 = new_events[0].0;
    let id2 = new_events[1].0;
    assert!(id1 < id2, "ids monotonic: {id1} < {id2}");
    assert!(f2.iter().any(|(_, e, _)| e == "ready"));

    // reconnect with Last-Event-ID = id1 -> replay only id2
    let f3 = sse_frames(&server, Some(id1), Duration::from_secs(4), 1).await;
    let replay: Vec<_> = f3.iter().filter(|(_, e, _)| e == "message.new").collect();
    assert_eq!(replay.len(), 1, "replay from after id1: {f3:?}");
    assert_eq!(replay[0].0, id2);

    // reconnect with lastEventId = id2 -> nothing new
    let f4 = sse_frames(&server, Some(id2), Duration::from_secs(2), 0).await;
    assert!(
        !f4.iter().any(|(_, e, _)| e == "message.new"),
        "no replay past id2: {f4:?}"
    );
}

/// Cold start (qqflow-server parity): with **no account registered at all**,
/// `/api/v1/push/messages` must still hand back a live stream — HTTP 200 plus
/// the `ready` baseline — instead of the old `503 no ready account`. Gating
/// here used to push downstream clients into a full reconnect-backoff cycle
/// for the entire registration + indexing window.
#[tokio::test]
async fn sse_connects_with_zero_accounts() {
    let dir = common::tmp_dir("ssezeroacct");
    let server = start_without_account(&dir).await;
    assert!(
        server.state.accounts.lock().is_empty(),
        "fixture must have no registered account"
    );

    let frames = sse_frames(&server, None, Duration::from_secs(2), 0).await;
    assert!(
        frames.iter().any(|(_, e, _)| e == "ready"),
        "zero-account stream still yields the ready baseline: {frames:?}"
    );
}

/// A client that connected before any account existed keeps receiving events
/// once one registers — and a *later* registration (the `error` -> corrected
/// path) does not orphan it. Both rely on the bus being global: when it lived
/// on `AccountHandle`, `register_account` minted a fresh channel and every
/// live subscriber silently stopped receiving anything, with no disconnect to
/// trigger a reconnect.
#[tokio::test]
async fn subscriber_survives_account_registration() {
    let dir = common::tmp_dir("ssesurvive");
    let server = start_without_account(&dir).await;

    let event = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "333".into(),
        source_name: "c".into(),
        group_name: Some("g".into()),
        content: "after registration".into(),
        timestamp: 1700000003,
        media: None,
    });

    // Subscribe first (zero accounts), then register an account and publish.
    let reader = sse_frames(&server, None, Duration::from_secs(8), 1);
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = server.storage.clone();
    let state = server.state.clone();
    let dir_owned = dir.clone();
    let writer = async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let info = AccountInfo {
            wxid: common::FAKE_WXID.to_string(),
            dir: dir_owned,
            db_storage: storage.clone(),
            session_db: Some(storage.join("session/session.db")),
        };
        let (_h, is_new) = server::register_account(
            &state,
            info,
            keystore::KeyMap::from(key),
            None,
        );
        assert!(is_new, "first registration for this wxid");
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Published on the global bus — the pre-registration subscriber must see it.
        state.events.send(event).ok();
    };
    let frames = tokio::join!(reader, writer).0;
    assert!(
        frames.iter().any(|(_, e, _)| e == "message.new"),
        "pre-registration subscriber receives post-registration events: {frames:?}"
    );
}
