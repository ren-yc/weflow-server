//! 嵌入者契约：**只用 `api::*`，不起 HTTP**。
//!
//! 这条测试的写法是刻意的：它把 `use` 限制在承诺面里。一旦它需要 `store::` / `db::` / `server::`
//! 里的任何东西，编译就会失败 —— 那正是"承诺面不够用"的信号，而不是测试的问题。
//!
//! 换句话说：**这条测试是承诺面的验收器**。它绿，说明嵌入者能用公开面把事做完。

use weflow_server::api;

mod common;

#[test]
fn an_embedder_can_read_an_account_without_http() {
    let dir = common::tmp_dir("embed-contract");
    let key = api::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);

    // ① 建索引 —— 嵌入者的第一个调用。
    let keys = api::KeyMap::Single(key);
    let index = api::open(&storage, &keys, common::FAKE_WXID).expect("建索引");
    assert!(!index.is_empty(), "夹具里应当有会话与消息");
    assert_eq!(index.wxid(), common::FAKE_WXID);

    // ② 读会话。顺序稳定，调用方能直接做 diff。
    let sessions = index.sessions();
    assert!(!sessions.is_empty());
    let names: Vec<&str> = sessions.iter().map(|s| s.username.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "会话按 username 升序");

    // ③ 读一个会话的消息，按时间升序。
    let group = sessions
        .iter()
        .find(|s| s.username == common::FAKE_GROUP)
        .expect("夹具里有那个群");
    let messages = index.messages(&group.username);
    assert!(!messages.is_empty(), "群里应当有消息");
    let times: Vec<i64> = messages.iter().map(|m| m.create_time).collect();
    let mut sorted_times = times.clone();
    sorted_times.sort();
    assert_eq!(times, sorted_times, "消息按时间升序");

    // ④ 群名片与联系人备注是**两个字段** —— 嵌入者容易混，所以承诺面把它们分开。
    let card = index.group_card(common::FAKE_GROUP, "wxid_member_b");
    assert_eq!(card, "四哥", "群名片来自 contact_fts 的影子表");
    let member = index
        .contacts()
        .into_iter()
        .find(|c| c.username == "wxid_member_b")
        .expect("联系人里有他");
    assert_eq!(member.display_name(), "李四", "联系人自己的名字");
    assert_ne!(card, member.display_name(), "两者不是同一个值");

    // ⑤ 群主。
    assert_eq!(index.chatroom_owner(common::FAKE_GROUP), "wxid_member_b");

    // ⑥ 会话类型判定不需要额外数据。
    assert_eq!(
        api::SessionKind::classify(common::FAKE_GROUP),
        api::SessionKind::Group
    );
}

/// 嵌入者要能**持续跟进**，而不只是读一次。
///
/// 这条路是 `api::Sync`：首次全量 → 增量 `poll_once` → `drain_events` → 用 `Index` 读。
/// 事件走自有队列而不是 tokio 的 `broadcast` —— 调用方不必处理 `Lagged`，也不必知道总线
/// 是 broadcast 还是别的什么。
#[test]
fn an_embedder_can_follow_updates_without_tokio() {
    let dir = common::tmp_dir("embed-sync");
    let key = api::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);
    let keys = api::KeyMap::Single(key);

    // ① 首次全量。
    let mut sync = api::Sync::open(&storage, &keys, common::FAKE_WXID).expect("首次全量");
    let before = sync.index().messages(common::FAKE_GROUP).len();
    assert!(before > 0);

    // ② 没有变化时，增量是廉价的（只比对时间戳），而且不产生事件。
    let (new, _revoked) = sync.poll_once().expect("空轮询");
    assert_eq!(new, 0, "没有新消息");
    assert!(sync.drain_events().is_empty(), "空轮询不产生事件");

    // ③ 往库里追加一条，再轮询 —— 索引里能看到，事件队列里也有提示。
    common::append_group_message_unique(&storage, &key.0, 1);
    let (new, _revoked) = sync.poll_once().expect("增量轮询");
    assert_eq!(new, 1, "追加的那条应当被发现");
    let after = sync.index().messages(common::FAKE_GROUP).len();
    assert_eq!(after, before + 1, "读到的条数随之增加");

    // 事件是**提示**：拿到它应当去读那一页，而不是把内容当权威。
    let events = sync.drain_events();
    assert!(
        events.iter().any(|e| matches!(e, api::Event::New(_))),
        "应当有一条 message.new 提示"
    );
    // 取走即消费：再取一次是空的。
    assert!(sync.drain_events().is_empty(), "事件取走即消费");
}
