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
use weflow_server::server::handlers::*;
use weflow_server::server::{self, AccountHandle};
use weflow_server::store::Store;
use weflow_server::sync::AccountSync;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct TestServer {
    addr: String,
    handle: Arc<AccountHandle>,
    state: Arc<server::AppState>,
}

async fn start(dir: &std::path::Path) -> TestServer {
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(dir, &key.0);
    let store = Arc::new(RwLock::new(Store::default()));
    let sync = Arc::new(Mutex::new(AccountSync::new(
        common::FAKE_WXID,
        &storage,
        keystore::KeyMap::from(key),
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
        store,
        events,
        sync,
        media_keys: None,
        history: Arc::new(Mutex::new(server::HistoryBuf::default())),
        watcher: Mutex::new(None),
    });
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
    let state = Arc::new(server::AppState {
        cfg,
        token: TOKEN.to_string(),
        accounts: parking_lot::Mutex::new(
            [(common::FAKE_WXID.to_string(), handle.clone())].into_iter().collect(),
        ),
        shutdown: shutdown_tx,
    });
    let app: Router = server::build_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer { addr: addr.to_string(), handle, state }
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
    });
    let event2 = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "222".into(),
        source_name: "b".into(),
        group_name: Some("g".into()),
        content: "world".into(),
        timestamp: 1700000002,
    });
    let reader = sse_frames(&server, None, Duration::from_secs(8), 2);
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.handle.events.send(event1).ok();
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.handle.events.send(event2).ok();
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