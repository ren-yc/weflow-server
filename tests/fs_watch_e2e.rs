//! Real end-to-end: file events (notify watcher) -> incremental sync ->
//! broadcast events (the same path the SSE push serves).

mod common;

use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

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
    let mut sync = AccountSync::new(common::FAKE_WXID, &storage, &dir.join("mirror"), weflow_server::keystore::KeyMap::from(key), store.clone(),
    );
    sync.full_sync().unwrap();
    assert_eq!(store.read().convs.len(), 2);

    let (events, mut rx) = broadcast::channel(1024);
    let sync = Arc::new(Mutex::new(sync));
    // swap the channel so the watcher broadcasts into our receiver
    {
        let mut guard = sync.lock();
        let new_tx = events.clone();
        guard.events = new_tx;
    }

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

    // expect a message.new event within a few seconds
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got: Option<Event> = None;
    while tokio::time::Instant::now() < deadline {
        match rx.recv().await {
            Ok(ev) => {
                if matches!(ev, Event::New(_)) {
                    got = Some(ev);
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    // rx tokens may have been consumed; drain any others
    let got = got.or_else(|| loop {
        match rx.try_recv() {
            Ok(ev) if matches!(ev, Event::New(_)) => break Some(ev),
            _ => break None,
        }
    });
    let got = got.expect("must receive a message.new event after the file write");

    match got {
        Event::New(m) => {
            assert_eq!(m.session_id, common::FAKE_GROUP);
            assert_eq!(m.session_type, "group");
            assert_eq!(m.rawid, "8299999999999999999");
            assert_eq!(m.content, "新消息");
            assert_eq!(m.timestamp, 1_700_000_200);
            assert!(!m.source_name.is_empty());
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // the store must contain the new row
    let guard = store.read();
    let conv = guard.convs.get(common::FAKE_GROUP).unwrap();
    assert_eq!(conv.len(), 5, "group conversation grew by one");
    assert!(
        conv.iter().any(|m| m.parsed.parsed_text == "新消息"),
        "new message indexed into the store"
    );

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}