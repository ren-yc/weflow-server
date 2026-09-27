//! 嵌入者示例：**不起 HTTP**，直接把一个账号读出来。
//!
//! 这个文件是承诺面的活文档 —— 它**只能用 `pub`**（example 是独立 crate，看不见 `pub(crate)`）。
//! 所以它编译得过，本身就说明一件事：**嵌入者要的东西都在公开面上**。
//!
//! ```text
//! cargo run --example embed --no-default-features -- weflow-server.json
//! ```
//!
//! 参数是**服务用的同一个配置文件**（`{ "db_path": …, "keys": { "contact/contact.db": "<hex>" } }`）——
//! 嵌入者手里通常就是它，不必再发明一种格式。
//!
//! `--no-default-features` 不是可选项：默认 feature 会把 axum 与 tokio 一起拉进来，而这个示例
//! 一行网络代码都没有。它同时也是**依赖面的证明** —— 这一行跑得起来，说明「不起 HTTP 也能用」
//! 不是一句口号。

use std::path::PathBuf;

use weflow_server::api;

fn main() -> anyhow::Result<()> {
    let Some(cfg_path) = std::env::args().nth(1) else {
        eprintln!("用法: cargo run --example embed --no-default-features -- <配置文件.json>");
        eprintln!();
        eprintln!("配置文件就是服务用的那一个：");
        eprintln!("  {{ \"wxid\": \"…\", \"db_path\": \"…/xwechat_files/<账号>\", \"keys\": {{ \"contact/contact.db\": \"<64位hex>\" }} }}");
        std::process::exit(2);
    };
    let raw = std::fs::read_to_string(&cfg_path)?;
    let cfg: serde_json::Value = serde_json::from_str(&raw)?;

    let account_root = PathBuf::from(cfg["db_path"].as_str().unwrap_or_default());
    let wxid = cfg["wxid"].as_str().unwrap_or("unknown");
    let keys: std::collections::HashMap<String, String> = cfg["keys"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        anyhow::bail!("配置里没有 keys —— 没有密钥就读不了任何库");
    }
    let keymap = api::KeyMap::from_parts(None, Some(keys))?;

    // ① 建索引。同步调用：真实账号是秒级到十几秒，放在哪个线程由调用方决定。
    let storage = account_root.join("db_storage");
    eprintln!("[embed] 索引 {} …", storage.display());
    let index = api::open(&storage, &keymap, wxid)?;
    eprintln!("[embed] 建好：{} 个会话", index.sessions().len());

    // ② 读会话。顺序是稳定的（按 username 升序），调用方可以直接做 diff。
    for session in index.sessions().iter().take(5) {
        let kind = match api::SessionKind::classify(&session.username) {
            api::SessionKind::Group => "群",
            api::SessionKind::Private => "私聊",
            api::SessionKind::Official => "公众号",
            api::SessionKind::Other => "其他",
        };
        let count = index.messages(&session.username).len();
        println!(
            "{kind}\t{}\t{count} 条",
            index.session_display(&session.username)
        );
    }

    // ③ 群名片与联系人备注是**两个字段** —— 承诺面把它们分开，因为混起来会显示错人。
    if let Some(group) = index
        .sessions()
        .into_iter()
        .find(|s| api::SessionKind::classify(&s.username) == api::SessionKind::Group)
    {
        let owner = index.chatroom_owner(&group.username);
        println!(
            "\n群 {} 的群主：{}",
            index.session_display(&group.username),
            if owner.is_empty() { "（未知）" } else { &owner }
        );
        for msg in index.messages(&group.username).iter().rev().take(3) {
            let card = index.group_card(&group.username, &msg.sender_username);
            println!(
                "  {}\t{}\t{}",
                index.session_display(&msg.sender_username),
                if card.is_empty() { "（无名片）" } else { &card },
                msg.create_time
            );
        }
    }

    Ok(())
}
