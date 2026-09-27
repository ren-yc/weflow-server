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
        weflow_server::api::SessionKind::Group
    );
}
