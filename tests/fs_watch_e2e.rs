//! Real end-to-end: file events (notify watcher) -> incremental sync ->
//! broadcast events (the same path the SSE push serves) —— 通过承诺面的 `drain_events()` 观察。

mod common;

use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use weflow_server::db::scan;
use weflow_server::keystore;
use weflow_server::store::Store;
use weflow_server::sync::watch::{self, WatchConfig};
use weflow_server::sync::{AccountSync, Event};

#[tokio::test]
async fn file_event_triggers_sync_and_message_event() {
    let dir = common::tmp_dir("watche2e");
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);

    let store = Arc::new(RwLock::new(Store::default()));
    let mut sync = AccountSync::new(
        common::FAKE_WXID,
        &storage,
        weflow_server::keystore::KeyMap::from(key),
        store.clone(),
    );
    sync.full_sync().unwrap();
    assert_eq!(store.read().convs.len(), 2);

    let sync = Arc::new(Mutex::new(sync));
    // **不换通道**：原来这里把 `AccountSync.events` 换成一个自建通道，好让自己的接收端收到
    // 事件 —— 那既动了实现面，又要调用方自己处理 `Lagged` / `Closed`。现在用承诺面上的
    // `drain_events()`：取走即消费，同步接口，需要的事件类型也从它自己带出来。

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let cfg = WatchConfig {
        debounce: Duration::from_millis(10),
        fallback: Some(Duration::from_millis(50)),
    };
    let handle = tokio::spawn(watch::spawn(sync.clone(), storage.clone(), cfg, shutdown_rx));

    // simulate WeChat writing a new message
    common::append_group_message(&storage, &key.0);
    let files_before = scan::enum_db_files(&storage);
    let _ = files_before;

    // Expect a message.new event within a few seconds.
    //
    // **超时必须套在 `recv()` 上，不能只做循环条件**：`recv().await` 会无限阻塞，
    // 所以「到点了没事件」原本表现为**永远挂住**而不是失败 —— 挂住会一直占着测试
    // 二进制（后续链接报 LNK1104）并让 CI 耗到 job 超时，比失败难查得多。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got: Option<Event> = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        // **超时必须能到点**：`drain_events` 是同步的，这里用 `sleep` 定节奏 —— 不阻塞、也不会
        // 像「无超时的 `recv().await`」那样永远挂住（挂住会占着测试二进制，后续链接报
        // LNK1104，并让 CI 耗到 job 超时，比失败难查得多）。
        let batch = sync.lock().drain_events();
        if let Some(ev) = batch.into_iter().find(|e| matches!(e, Event::New(_))) {
            got = Some(ev);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let got = got.expect("must receive a message.new event after the file write");

    match got {
        Event::New(m) => {
            assert_eq!(m.session_id, common::FAKE_GROUP);
            assert_eq!(m.session_type, "group");
            assert_eq!(m.rawid, "8299999999999999999");
            assert_eq!(m.content, "新消息");
            assert_eq!(m.timestamp, 1_700_000_200);
            assert!(!m.source_name.is_empty());
            // group events carry the resolved group name (not the raw id)
            assert_eq!(m.group_name.as_deref(), Some("项目群"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // the store must contain the new row.
    //
    // Scoped so the read guard is dropped before the await below: a
    // `std::sync` guard is not Send, so holding one across an await point makes
    // the future unable to migrate between workers — harmless in this
    // current-thread test, a deadlock waiting to happen in anything copied from
    // it.
    {
        let guard = store.read();
        let conv = guard.convs.get(common::FAKE_GROUP).unwrap();
        assert_eq!(conv.len(), 5, "group conversation grew by one");
        assert!(
            conv.iter().any(|m| m.parsed.parsed_text == "新消息"),
            "new message indexed into the store"
        );
    }

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}
