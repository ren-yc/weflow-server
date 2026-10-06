//! SSE Last-Event-ID replay integration test: real HTTP server + two SSE
//! clients, verifying replay (1000/10min buffer) and incremental ids.

mod common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tower::ServiceExt;
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
        state.bus.clone(),
    )));
    sync.lock().full_sync().unwrap();

    let info = AccountInfo {
        wxid: common::FAKE_WXID.to_string(),
        dir: dir.to_path_buf(),
        db_storage: storage.clone(),
        session_db: Some(storage.join("session/session.db")),
    };
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
                if let Some(v) = l.strip_prefix("id:") {
                    cur_id = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = l.strip_prefix("event:") {
                    cur_ev = v.trim().to_string();
                } else if let Some(v) = l.strip_prefix("data:") {
                    cur_data.push_str(v.trim());
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
        server.state.bus.publish(event1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.state.bus.publish(event2);
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

/// 历史由**生产者**单点写入，两半失败模式都要钉住：
/// ① 没有任何 SSE 连接在线时广播没有接收者 —— 修复前由订阅端各自 append，零订阅者
///    意味着**没有人在写历史**，这段时间的事件从此不在重放窗口里，之后带
///    `Last-Event-ID` 重连的客户端漏收却无从得知；
/// ② N 个订阅者在线时同一条被写 N 次，id 随连接数跳号，1000 条的窗口被重复条目稀释。
#[tokio::test]
async fn sse_history_is_recorded_once_without_subscribers() {
    let dir = common::tmp_dir("ssehist-single-writer");
    let server = start(&dir).await;
    let ev = |rawid: &str, ts: i64| {
        weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
            session_id: common::FAKE_GROUP.to_string(),
            session_type: "group",
            rawid: rawid.into(),
            source_name: "a".into(),
            group_name: Some("g".into()),
            content: "hello".into(),
            timestamp: ts,
            media: None,
        })
    };

    // ① 此刻没有任何 SSE 连接（start() 只起了服务）：publish 仍必须写历史。
    let before = server.state.bus.history().lock().replay_since(0).len();
    let id1 = server.state.bus.publish(ev("7001", 1_700_000_010));
    let id2 = server.state.bus.publish(ev("7002", 1_700_000_011));
    let after = server.state.bus.history().lock().replay_since(0);
    assert_eq!(
        after.len(),
        before + 2,
        "零订阅者时 publish 也必须写历史：漏一条 = 重连后那条永久消失",
    );
    assert_eq!(id2, id1 + 1, "每条事件只分配一个 id（订阅端各自写时会随连接数跳号）");

    // ② 新连接的重放必须用**生产者分配的那对 id**；带着 id2 重连不得再收到这两条。
    // 采到两条 message.new 为止（expected_new=0 会在第一帧 ready 就收摊，测不到重放）。
    let f = sse_frames(&server, None, Duration::from_secs(3), 2).await;
    let news: Vec<u64> = f.iter().filter(|(_, e, _)| e == "message.new").map(|(id, _, _)| *id).collect();
    assert!(
        news.contains(&id1) && news.contains(&id2),
        "重放帧的 id 必须是生产者的编号（订阅端自己编号时历史里已有同号条目，两边会分叉）: {f:?}",
    );
    // 反向：永不满足 expected_new ⇒ 整窗读满，断言才是真「这段时间没有任何重放」。
    let f2 = sse_frames(&server, Some(id2), Duration::from_secs(2), 5).await;
    let news2: Vec<u64> = f2.iter().filter(|(_, e, _)| e == "message.new").map(|(id, _, _)| *id).collect();
    assert!(
        !news2.contains(&id1) && !news2.contains(&id2),
        "带 Last-Event-ID={id2} 重连不得重复收到这两条: {f2:?}",
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
        assert!(
            matches!(
                server::register_account(&state, info, keystore::KeyMap::from(key), None),
                server::BindOutcome::Bound(_)
            ),
            "first registration for this wxid"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Published on the global bus — the pre-registration subscriber must see it.
        state.bus.publish(event);
    };
    let frames = tokio::join!(reader, writer).0;
    assert!(
        frames.iter().any(|(_, e, _)| e == "message.new"),
        "pre-registration subscriber receives post-registration events: {frames:?}"
    );
}

/// SSE 载荷的**键集**与「媒体里没有什么」都是契约 —— 而 SSE 是流式接口，golden 快照
/// 覆盖不到它（它是长连接，不是一次请求一次响应）。所以这条断言就是它的护栏。
///
/// 特别地：内部事件里的 `MediaHint` **带着 `aes_key`**，而推送载荷绝不能带上它 ——
/// 那是解密用的密钥，进了 SSE 就等于进了每个订阅方的日志与浏览器内存。
#[tokio::test]
async fn sse_payload_keys_are_pinned() {
    let dir = common::tmp_dir("sseshape");
    let server = start(&dir).await;

    let ev = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "shape-1".into(),
        source_name: "src".into(),
        group_name: Some("项目群".into()),
        content: "hi".into(),
        timestamp: 1_700_000_001,
        // 从一个**带 `aes_key` 的内部提示**构造，用来验证那条 `From` 真的把密钥丢掉了。
        // 类型 `PushMedia` 本身就没有这个字段 —— 也就是说这条保证由类型系统兜底，
        // 下面的断言是第二道锁。
        media: Some(weflow_server::sync::PushMedia::from(
            &weflow_server::parser::MediaHint {
                kind: weflow_server::parser::MediaKind::Image,
                file_name: "a.jpg".into(),
                md5: Some("deadbeef".into()),
                aes_key: Some("SECRET-AES-KEY".into()),
            },
        )),
    });
    let reader = sse_frames(&server, None, Duration::from_secs(8), 1);
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.state.bus.publish(ev);
    };
    let frames = tokio::join!(reader, sender).0;
    let (_, _, data) = frames
        .iter()
        .find(|(_, e, _)| e == "message.new")
        .expect("a message.new frame");
    let v: serde_json::Value = serde_json::from_str(data).expect("payload is JSON");

    // 键集：多一个少一个都是契约变更。
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "content",
            "event",
            "groupName",
            "media",
            "rawid",
            "sessionId",
            "sessionType",
            "sourceName",
            "timestamp"
        ],
        "message.new 的键集是契约：{v}"
    );

    // media 是**第三种形状**：只有元数据，没有路径。取字节用的键是 `mediaId`，
    // 它**只在导出根下确有该文件时才出现**（见下面那条正路径测试）—— 本用例的夹具
    // 没有导出任何文件，因此这里钉的是「没有 mediaId」的那一支。
    let media = v["media"].as_object().expect("media object");
    let mut mkeys: Vec<&str> = media.keys().map(String::as_str).collect();
    mkeys.sort_unstable();
    assert_eq!(
        mkeys,
        ["fileName", "md5", "type"],
        "未导出时 SSE 的 media 只有元数据：{media:?}"
    );

    // 密钥绝不出现 —— 查**整帧原文**，而不是只看解析后的键：
    // 藏在某个值里的密钥同样是泄露。
    assert!(!data.contains("SECRET-AES-KEY"), "推送载荷里出现了 aes_key：{data}");
    assert!(!data.contains("aes"), "推送载荷里出现了 aes 字样：{data}");
}
/// 发一个最小 HTTP GET，返回 (状态码, 正文)。媒体是二进制，不能用 `read_to_string`。
async fn http_get(addr: &str, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf);
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body_at = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (status, buf[body_at..].to_vec())
}

/// `mediaId` 的**正路径**：导出根下确有文件时必须出现，而且**出现即可取**。
///
/// 为什么单列一条：上面那条键集断言只在「夹具恰好没有导出文件」这一取值上通过
/// ——它钉住的是**没有** mediaId 的形状，而取字节的入口恰恰是**有** mediaId 的那一支。
/// 单元测试只能证明「一次 stat 的判据对」，证明不了「从导出文件到 SSE 帧、再到
/// HTTP 取回字节」这条链是通的：链上任何一环断了，都只有端到端看得见。
#[tokio::test]
async fn sse_media_id_is_advertised_and_fetchable() {
    let dir = common::tmp_dir("ssemedia");
    let server = start(&dir).await;

    // 导出根下摆好文件：<export_dir>/<session>/images/<file> —— 布局由
    // `media::export` 决定，这里必须与它一致（不一致就会得到「没有 mediaId」）。
    let file_name = "aabbccddeeff00112233445566778899.jpg";
    let exported = dir.join("api-media").join(common::FAKE_GROUP).join("images");
    std::fs::create_dir_all(&exported).unwrap();
    let bytes: &[u8] = b"\xFF\xD8 sse jpeg \xFF\xD9";
    std::fs::write(exported.join(file_name), bytes).unwrap();

    let ev = weflow_server::sync::Event::New(weflow_server::sync::NewMessageEvent {
        session_id: common::FAKE_GROUP.to_string(),
        session_type: "group",
        rawid: "media-1".into(),
        source_name: "src".into(),
        group_name: Some("项目群".into()),
        content: "[图片]".into(),
        timestamp: 1_700_000_002,
        media: Some(weflow_server::sync::PushMedia::from(
            &weflow_server::parser::MediaHint {
                kind: weflow_server::parser::MediaKind::Image,
                file_name: file_name.into(),
                md5: Some("deadbeef".into()),
                aes_key: Some("SECRET-AES-KEY".into()),
            },
        )),
    });
    let reader = sse_frames(&server, None, Duration::from_secs(8), 1);
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.state.bus.publish(ev);
    };
    let frames = tokio::join!(reader, sender).0;
    let (_, _, data) = frames
        .iter()
        .find(|(_, e, _)| e == "message.new")
        .expect("a message.new frame");
    let v: serde_json::Value = serde_json::from_str(data).expect("payload is JSON");
    let media_id = v["media"]["mediaId"]
        .as_str()
        .unwrap_or_else(|| panic!("导出根下有文件时 mediaId 必须出现：{v}"));
    assert!(!media_id.is_empty(), "mediaId 不得为空串");
    // 密钥的护栏在这条路径上同样成立。
    assert!(!data.contains("SECRET-AES-KEY"), "推送载荷里出现了 aes_key：{data}");

    // 「出现即可取」：拿这个 id 真的把字节取回来。
    let (status, body) = http_get(&server.addr, &format!("/api/v1/media/{media_id}")).await;
    assert_eq!(status, 200, "mediaId 是从该端点取的：{media_id}");
    assert_eq!(body, bytes, "取回的字节必须与导出的一致");
}

/// `sync` 基线帧的**键集**是契约。
///
/// 这条护栏此前不存在 —— `sync` 的载荷改动了在仓库里是静默的（上面那条只钉 `message.new`）。
/// 它同时钉住「**连接建立就发一帧基线**」：没有那一帧，客户端在「连上」到「第一次水位变化」之间
/// 是盲的，而这中间可能很长（账号空闲、或还没注册账号）。
#[tokio::test]
async fn sync_baseline_keys_are_pinned() {
    let dir = common::tmp_dir("ssesync");
    let server = start(&dir).await;

    // 新连接：`ready` 之后应立刻收到一帧 `sync` 基线。
    let frames = sse_frames(&server, None, Duration::from_secs(8), 1).await;
    let (_, _, data) = frames
        .iter()
        .find(|(_, e, _)| e == "sync")
        .unwrap_or_else(|| panic!("连接后应立刻有一帧 sync 基线，实际拿到：{frames:?}"));
    let v: serde_json::Value = serde_json::from_str(data).expect("payload is JSON");

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["event", "generation", "watermarks"], "sync 的键集是契约：{v}");
    assert_eq!(v["event"], "sync");
    assert!(v["generation"].as_u64().is_some(), "generation 是非负整数：{v}");
    assert!(v["watermarks"].is_array(), "watermarks 是数组：{v}");
    // 基线帧**只**说水位：它不该看起来像一条消息。
    for leaked in ["content", "sessionId", "rawid", "media"] {
        assert!(v.get(leaked).is_none(), "基线帧不该带 {leaked}：{v}");
    }
}

/// 通知面的**撤回帧**带上平台消息号。
///
/// 规范里 `platformMessageId` 是可选的，而这条通道的用法是「收到通知后去拉那一页」——
/// 撤回帧不给它，客户端就只剩时间戳可猜。新消息帧仍不给：那要在推送热路径上逐事件查一次
/// 索引。两种帧的差别只在一个值上，所以必须分别钉住（键集断言看不出这个差别）。
#[tokio::test]
async fn chatlab_revoke_frame_carries_platform_message_id() {
    let dir = common::tmp_dir("sserevoke");
    let server = start(&dir).await;

    // 打开的是**通知面**（不是老面 /api/v1/push/messages）。
    let req = format!(
        "GET /chatlab/push/messages?access_token={TOKEN} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
        server.addr
    );
    let mut stream = TcpStream::connect(&server.addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(stream);
    // 消费 HTTP 头
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
            break;
        }
    }

    // 订阅建立之后再发事件：连接建立前发出的事件不会进这条流的重放缓冲。
    tokio::time::sleep(Duration::from_millis(300)).await;
    server
        .state
        .bus
        .publish(weflow_server::sync::Event::Revoke(weflow_server::sync::RevokeEvent {
            session_id: common::FAKE_GROUP.to_string(),
            session_type: "group",
            rawid: "9001".into(),
            source_name: "src".into(),
            group_name: Some("项目群".into()),
            content: "对方撤回了一条消息".into(),
            timestamp: 1_700_000_005,
        }));

    // 第一帧是基线 `sync`，所以一直读到 `message.revoke` 为止。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut event = String::new();
    let mut data = String::new();
    let mut saw_revoke = false;
    while !saw_revoke {
        let mut line = String::new();
        let read = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            reader.read_line(&mut line),
        )
        .await;
        match read {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => panic!("在 8 秒内没有收到 message.revoke 帧"),
            Ok(Ok(_)) => {
                let l = line.trim_end();
                if let Some(v) = l.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = l.strip_prefix("data:") {
                    data.push_str(v.trim());
                } else if l.is_empty() {
                    if event == "message.revoke" {
                        saw_revoke = true;
                    } else {
                        event.clear();
                        data.clear();
                    }
                }
            }
        }
    }

    let v: serde_json::Value = serde_json::from_str(&data).expect("payload is JSON");
    assert_eq!(v["platformMessageId"], "9001", "撤回帧必须带平台消息号：{v}");
    assert_eq!(v["eventId"], "9001", "事件 id 与平台号同值但不同义：{v}");
    assert_eq!(v["event"], "message.revoke");
    assert!(v.get("content").is_none(), "通知帧不带正文：{v}");
}

/// 流的**最后一批字节**必须以 SSE 的空行分隔符结尾（\n\n）。
///
/// 为什么钉它：SSE 的帧分隔是「空行」，而最后一帧后面没有下一帧来触发分隔写入时，
/// 尾帧是否自带分隔完全取决于 axum 的编码行为——它随版本可能改变，升级是静默的。
/// 最后一帧丢分隔时，按块解析的消费者要靠 EOF 冲刷才不丢尾帧（clients/python 行为层
/// 的 `watch` 正是这么兜底的），按行解析的消费者则会**静默丢弃**没有换行收尾的
/// 最后一行。这里把「线上的尾字节形状」钉住：它变红时说明分隔语义变了，上述消费者
/// 都需要复核。
///
/// 经 tower `oneshot` 读响应体（与 api_smoke 同路），`to_bytes` 拿到的是**解码后**的
/// 正文，断言不受 chunked 帧化的干扰。
#[tokio::test]
async fn stream_tail_ends_with_a_blank_line_separator() {
    let dir = common::tmp_dir("ssetail");
    let server = start(&dir).await;
    let app: Router = server::build_router(server.state.clone());
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/v1/push/messages?access_token={TOKEN}"))
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The stream closes itself via the shutdown watch: trigger it, then read
    // the body to EOF.
    server.state.shutdown.send_replace(true);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("response body");
    let body = &body[..];
    assert!(
        body.starts_with(b"event: ready"),
        "流应从 ready 帧开始：{:?}",
        String::from_utf8_lossy(&body[..body.len().min(80)]),
    );
    assert!(
        body.ends_with(b"\n\n"),
        "SSE 流的最后一帧必须自带空行分隔，否则按空行分块的消费者会丢尾帧：尾 40 字节 = {:?}",
        String::from_utf8_lossy(&body[body.len().saturating_sub(40)..]),
    );
}