//! 客户端面的验收（可执行版本）：**真实二进制** 对上一个桩 HTTP 服务端。
//!
//! 覆盖三件在单元/契约测试里覆盖不到的事：
//! 1. **全部子命令**都能对着一个可用的服务跑通（退出码 0 ＋ 关键输出）；
//! 2. **MCP 三步演示**（列群 → 搜 → 取）走真实 stdio JSON-RPC，而不是直接调函数；
//! 3. `--rows` 的 **RSS 平坦**断言（`#[ignore]`，验收时显式跑 —— 造大语料很慢）。
//!
//! 为什么用桩服务端而不是夹具真库：这里要验的是**客户端编排**（CLI 与 MCP 的接线、退出码、stdout
//! 形状），不是解析或索引；桩让每条断言只依赖一个自己写死的响应。夹具真库那条路由
//! `api_smoke`／`conformance_runner` 覆盖。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const TOKEN: &str = "e2e-token-0123456789abcdef";
const TALKER: &str = "wxid_demo";

fn session_json() -> Value {
    json!({
        "displayName": "演示会话", "lastTimestamp": 1_700_000_000, "messageCount": 1,
        "sessionType": "private", "summary": null, "type": 0, "unreadCount": 0,
        "username": TALKER,
    })
}

fn native_message() -> Value {
    json!({
        "appmsgSubtype": null, "baseType": 1, "content": "hi", "createTime": 1_700_000_000,
        "isSend": 1, "localId": 7, "localType": 3,
        "media": {"fileName": "abc.png", "mediaId": "abc123", "md5": "d41d8", "type": "image"},
        "parsedContent": "hi", "quote": null, "rawContent": "<msg>hi</msg>",
        "replyToMessageId": "41", "senderName": "张三", "senderUsername": "alice",
        "serverId": "42", "sortSeq": 1,
    })
}

fn pull_message() -> Value {
    json!({
        "accountName": "alice", "content": "hi", "groupNickname": "",
        "platformMessageId": "42", "sender": "alice", "timestamp": 1_700_000_000, "type": 1,
    })
}

/// 起一个桩服务端，返回 base URL。
async fn spawn_stub() -> String {
    let app = Router::new()
        .route("/api/v1/accounts", get(accounts))
        .route("/api/v1/sync", post(sync))
        .route("/api/v1/sessions", get(sessions))
        .route("/api/v1/sessions/{id}/messages", get(pull))
        .route("/api/v1/messages", get(messages))
        .route("/api/v1/contacts", get(contacts))
        .route("/chatlab/messages", get(chatlab));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn accounts() -> Json<Value> {
    Json(json!({"success": true, "accounts": [{
        "wxid": TALKER, "db_storage": "", "message_count": 1, "state": "ready",
    }]}))
}

async fn sync() -> Json<Value> {
    Json(json!({"success": true, "newMessages": 1, "revokeMessages": 0}))
}

async fn sessions(axum::extract::RawQuery(q): axum::extract::RawQuery) -> Json<Value> {
    // `list_all_sessions` 翻页：第一页给一条，offset 推进后的空页终止循环。
    let raw = q.unwrap_or_default();
    if raw.contains("offset=1") {
        Json(json!({"success": true, "count": 0, "sessions": []}))
    } else {
        Json(json!({"success": true, "count": 1, "sessions": [session_json()]}))
    }
}

async fn pull() -> Json<Value> {
    Json(json!({
        "chatlab": {"version": "0.0.2", "generator": "stub", "exportedAt": 1},
        "members": [], "messages": [pull_message()],
        "meta": {"groupId": "", "name": "演示会话", "ownerId": "", "platform": "wechat", "type": "private"},
        "sync": {"hasMore": false, "nextSince": 1_700_000_000, "nextOffset": 0, "watermark": 1_700_000_000},
    }))
}

async fn messages() -> Json<Value> {
    Json(json!({
        "success": true, "count": 1, "hasMore": false, "talker": TALKER,
        "media": {"count": 0, "enabled": false, "exportPath": ""},
        "messages": [native_message()],
    }))
}

async fn contacts() -> Json<Value> {
    Json(json!({"success": true, "count": 1, "total": 1, "hasMore": false, "contacts": [{
        "alias": "", "avatarUrl": "", "displayName": "张三", "nickname": "三儿",
        "remark": "客户张三", "type": "friend", "username": "alice",
    }]}))
}

async fn chatlab() -> Json<Value> {
    Json(json!({
        "chatlab": {"version": "0.0.2", "generator": "stub", "exportedAt": 1},
        "count": 1, "members": [], "messages": [pull_message()],
        "meta": {"groupId": "", "name": "演示会话", "ownerId": "", "platform": "wechat", "type": "private"},
        "page": {"hasMore": false, "nextCursor": null}, "talker": TALKER,
    }))
}

fn run(base: &str, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_weflow-server"))
        .args(args)
        .env("WEFLOW_BASE_URL", base)
        .env("WEFLOW_TOKEN", TOKEN)
        .env_remove("WEFLOW_EMBED_CONFIG")
        .output()
        .expect("spawn weflow-server");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// 验收 1：**全部子命令**对着一个可用服务跑通。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_subcommand_works_against_a_live_service() {
    let base = spawn_stub().await;
    for (args, want) in [
        (vec!["sessions", "--json"], "演示会话"),
        (vec!["messages", "--talker", TALKER, "--json"], "张三"),
        (vec!["search", "--talker", TALKER, "--keyword", "hi", "--json"], "hi"),
        (vec!["contacts", "--json"], "三儿"),
        (vec!["accounts", "--json"], "ready"),
        (vec!["sync", "--json"], "newMessages"),
    ] {
        let (code, stdout, stderr) = run(&base, &args);
        assert_eq!(code, 0, "{args:?} 应退出 0；stderr: {stderr}");
        assert!(stdout.contains(want), "{args:?} 的输出应含 {want:?}，实际: {stdout}");
    }
}

/// 验收 1b：`export` 落盘（同样对着桩服务端）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_writes_a_file_against_a_live_service() {
    let base = spawn_stub().await;
    let dir = std::env::temp_dir().join(format!("weflow-e2e-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (code, stdout, stderr) = run(&base, &["export", "--out", dir.to_str().unwrap(), "--format", "jsonl"]);
    assert_eq!(code, 0, "export 应退出 0；stdout: {stdout} stderr: {stderr}");
    let files: Vec<String> = std::fs::read_dir(&dir)
        .expect("导出目录应存在")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(files.iter().any(|f| f.ends_with(".jsonl")), "应写出 jsonl，实际: {files:?}");
    assert!(files.iter().any(|f| f == "index.json"), "应写出 index.json，实际: {files:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- MCP 三步演示 ---------------------------------------------------------

fn send(stdin: &mut impl Write, v: Value) {
    writeln!(stdin, "{v}").expect("写 MCP 请求");
    stdin.flush().expect("flush");
}

fn read_until(reader: &mut impl BufRead, id: i64) -> Value {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("读 MCP 响应");
        assert!(n > 0, "MCP 服务端在回复 id={id} 之前就关闭了 stdout");
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.get("id").and_then(Value::as_i64) == Some(id) {
            return v;
        }
    }
}

/// 验收 2：**MCP 三步演示** —— 列群 → 搜 → 取，走真实 stdio JSON-RPC。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_three_step_demo_lists_searches_and_fetches() {
    let base = spawn_stub().await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_weflow-server"))
        .arg("mcp")
        .env("WEFLOW_BASE_URL", &base)
        .env("WEFLOW_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn weflow-server mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    // 握手：协议版本用「仍有 initialize 握手」的最新版（2026-07-28 起是 discover 生命周期）。
    send(&mut stdin, json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "0.1.0"},
        },
    }));
    let init = read_until(&mut out, 1);
    assert!(init["result"]["capabilities"]["tools"].is_object(), "应声明 tools 能力: {init}");
    assert!(init["result"]["instructions"].as_str().unwrap_or("").contains("离开本机"),
        "instructions 必须带数据离机提示: {init}");
    send(&mut stdin, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = read_until(&mut out, 2);
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools 是数组")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    for want in ["list_sessions", "search_messages", "get_messages"] {
        assert!(names.iter().any(|n| n == want), "工具集应含 {want}，实际: {names:?}");
    }

    // 一步：列会话。
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "list_sessions", "arguments": {}}}));
    let listed = read_until(&mut out, 3);
    assert_ne!(listed["result"]["isError"], json!(true), "list_sessions 不该是工具级错误: {listed}");
    let text = listed["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains(TALKER) || text.contains("演示会话"), "列会话应给出会话: {listed}");

    // 二步：搜。
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "search_messages", "arguments": {"talker": TALKER, "keyword": "hi"}}}));
    let searched = read_until(&mut out, 4);
    assert_ne!(searched["result"]["isError"], json!(true), "search 不该是工具级错误: {searched}");
    assert!(searched["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("hi"),
        "搜索结果应含命中消息: {searched}");

    // 三步：取消息。
    send(&mut stdin, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "get_messages", "arguments": {"talker": TALKER}}}));
    let fetched = read_until(&mut out, 5);
    assert_ne!(fetched["result"]["isError"], json!(true), "get_messages 不该是工具级错误: {fetched}");
    assert!(fetched["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("42"),
        "取消息应含 platformMessageId: {fetched}");

    child.kill().ok();
    let _ = child.wait();
}

// ---- --rows 的 RSS 平坦（验收时显式跑）--------------------------------------

/// 验收 3：大语料下**峰值 RSS 与条数无关**。
///
/// `#[ignore]`：造 20 万行要几十秒，不适合每次 `cargo test`。验收时显式跑：
///   cargo test --locked --features testing --test cli_e2e -- --ignored --nocapture
///
/// 断言取 `[rss]` 的**首/末**采样点，要求末点相对首点的增量小于一成。
#[test]
#[ignore = "验收专用：造大语料很慢"]
fn export_rows_keeps_peak_rss_flat() {
    let dir = std::env::temp_dir().join(format!("weflow-e2e-rows-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_weflow-server"))
        .args(["export", "--out", dir.to_str().unwrap(), "--format", "jsonl", "--rows", "200000"])
        .env_remove("WEFLOW_BASE_URL")
        .env_remove("WEFLOW_TOKEN")
        .output()
        .expect("spawn weflow-server");
    assert!(out.status.success(), "export --rows 应退 0；stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    // 采样行是 `[rss] session=<i> rows=<n> peak_kb=Some(<kb>)`；取不到峰值时打印 `None`，那种点跳过。
    let samples: Vec<(u64, u64)> = text
        .lines()
        .filter(|l| l.starts_with("[rss]"))
        .filter_map(|l| {
            let rows = l.split("rows=").nth(1)?.split(' ').next()?.parse::<u64>().ok()?;
            let raw = l.rsplit("peak_kb=").next()?.trim();
            let peak = raw.strip_prefix("Some(")?.strip_suffix(')')?.parse::<u64>().ok()?;
            Some((rows, peak))
        })
        .collect();
    // 只比**大**会话：夹具里排在前面的是种子会话（几条消息），它们的采样点是 warm-up —— 拿它们当
    // 「起点」会把「第一次导入一个大群」的一次性开销当成增长。内存恒定要看的是**大群之间**。
    let big: Vec<u64> = samples
        .iter()
        .filter(|(rows, _)| *rows >= 1_000)
        .map(|(_, peak)| *peak)
        .collect();
    assert!(big.len() >= 2, "至少要有两个大会话的采样点，实际: {samples:?}\n{text}");
    let first = big[0];
    let peak = *big.iter().max().unwrap();
    assert!(
        peak < first + first / 10,
        "峰值 RSS 应平坦：首个大群 {first} KB → 全局峰值 {peak} KB（增量超过一成）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
