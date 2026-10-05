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
        // 媒体句柄：`--with-media` 要据此下载字节，而服务端只对**内容摘要命名**的句柄保证可取。
        "media": {"fileName": "deadbeef.png", "type": "image"},
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
        .route("/api/v1/media/{id}", get(media))
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

/// 媒体字节：`--with-media` 的下载目标。
///
/// `boom.png` 刻意回 **503**（不是 404）：用来钉「取字节失败必须响亮失败」—— 把瞬时 5xx 也当成
/// 「不是可取句柄」会静默少下载若干媒体、而整体仍退 0。
async fn media(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (axum::http::StatusCode, Vec<u8>) {
    if id == "boom.png" {
        // 5xx：这次没拿到 —— 必须上抛（见 `with_media_fails_loudly_when_bytes_fetch_errors`）。
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, Vec::new())
    } else if id == "gone.png" {
        // 404：这个句柄本来就取不到 —— 必须跳过，不让整轮失败（见 404 对照用例）。
        (axum::http::StatusCode::NOT_FOUND, Vec::new())
    } else {
        (axum::http::StatusCode::OK, b"PNG-BYTES".to_vec())
    }
}

async fn chatlab(axum::extract::RawQuery(q): axum::extract::RawQuery) -> Json<Value> {
    let raw = q.unwrap_or_default();
    // `keyword=big` 时给一页**远超字符预算**的消息，用来钉「截断时 hasMore 必须为真」与
    // 「按 nextOffset 续拉确实前进」。所以这里**尊重 `offset`**：从 offset 起给出剩下的消息。
    if raw.contains("keyword=big") {
        let offset: usize = raw
            .split('&')
            .find_map(|kv| kv.strip_prefix("offset="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let messages: Vec<Value> = (offset..60)
            .map(|i| {
                json!({
                    "accountName": "alice", "content": "x".repeat(2000), "groupNickname": "",
                    "platformMessageId": format!("{i}"), "sender": "alice",
                    "timestamp": 1_700_000_000, "type": 0,
                })
            })
            .collect();
        let more = offset + messages.len() < 60;
        return Json(json!({
            "chatlab": {"version": "0.0.2", "generator": "stub", "exportedAt": 1},
            "count": messages.len(), "members": [], "messages": messages,
            "meta": {"groupId": TALKER, "name": "演示会话", "ownerId": "", "platform": "wechat", "type": "group"},
            "page": {"hasMore": more, "nextCursor": null}, "talker": TALKER,
        }));
    }
    if raw.contains("talker=boom-talker") {
        // 该会话的媒体**取不到字节**（见 `media` 的 503 分支），用来验 --with-media 的失败口径。
        let mut m = pull_message();
        m["media"] = json!({"fileName": "boom.png", "type": "image"});
        return Json(json!({
            "chatlab": {"version": "0.0.2", "generator": "stub", "exportedAt": 1},
            "count": 1, "members": [], "messages": [m],
            "meta": {"groupId": "", "name": "演示会话", "ownerId": "", "platform": "wechat", "type": "private"},
            "page": {"hasMore": false, "nextCursor": null}, "talker": "boom-talker",
        }));
    }
    if raw.contains("talker=gone-talker") {
        // 与 boom 相反：这个句柄回 **404**（本来就取不到），用来做「404 放过、5xx 上抛」的对照。
        let mut m = pull_message();
        m["media"] = json!({"fileName": "gone.png", "type": "image"});
        return Json(json!({
            "chatlab": {"version": "0.0.2", "generator": "stub", "exportedAt": 1},
            "count": 1, "members": [], "messages": [m],
            "meta": {"groupId": "", "name": "演示会话", "ownerId": "", "platform": "wechat", "type": "private"},
            "page": {"hasMore": false, "nextCursor": null}, "talker": "gone-talker",
        }));
    }
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
        // 查桩**自有**的字段（发信人名）而不是请求里的关键词 —— 后者被回显就满足，等于没查。
        (vec!["search", "--talker", TALKER, "--keyword", "hi", "--json"], "张三"),
        (vec!["contacts", "--json"], "三儿"),
        (vec!["accounts", "--json"], "ready"),
        // 查**值**而不只是键名：只回显键名也含 "newMessages"。
        (vec!["sync", "--json"], "newMessages"),
    ] {
        let (code, stdout, stderr) = run(&base, &args);
        assert_eq!(code, 0, "{args:?} 应退出 0；stderr: {stderr}");
        assert!(stdout.contains(want), "{args:?} 的输出应含 {want:?}，实际: {stdout}");
    }
    // 键名在场不等于值被转发：解析出来断言**值**（`--json` 是美化输出，字符串比较会依赖空格）。
    let v: Value = serde_json::from_str(&run(&base, &["sync", "--json"]).1).expect("--json 应是 JSON");
    assert_eq!(v["newMessages"], json!(1), "sync 应转发服务端给的新消息数: {v}");
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
    // 文件名「存在」不说明写了什么：一个空 jsonl 也满足上面两条。
    let jsonl = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("应有 jsonl");
    let body = std::fs::read_to_string(&jsonl).unwrap();
    assert!(body.contains("\"_type\":\"header\""), "首行应是 header: {body}");
    assert!(body.contains("hi"), "导出物应含桩消息的正文: {body}");
    // 本仓的会话类型由 talker 推导（`wxid_demo` 不是群），所以这里应是 private。
    assert!(body.contains("\"type\":\"private\""), "meta.type 应由 talker 推导: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 验收 1c：`--with-media` 把字节落盘，且导出物里的句柄就是落盘名。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_with_media_lands_bytes_and_keeps_the_handle() {
    let base = spawn_stub().await;
    let dir = std::env::temp_dir().join(format!("weflow-e2e-withmedia-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (code, stdout, stderr) = run(
        &base,
        &["export", "--out", dir.to_str().unwrap(), "--format", "jsonl", "--with-media"],
    );
    assert_eq!(code, 0, "export --with-media 应退出 0；stdout: {stdout} stderr: {stderr}");
    let file = dir.join("media").join("deadbeef.png");
    assert!(file.exists(), "字节应落到 media/ 下，实际目录: {:?}", std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>()));
    assert_eq!(std::fs::read(&file).unwrap(), b"PNG-BYTES", "落盘的应是媒体字节");
    let jsonl = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("应有 jsonl");
    let body = std::fs::read_to_string(&jsonl).unwrap();
    // **两个集合必须相等**：只断言桩自带的那一个串，透传实现也会通过。
    let handles: std::collections::BTreeSet<String> = body
        .split("\"fileName\":\"")
        .skip(1)
        .filter_map(|s| s.split('"').next().map(str::to_string))
        .collect();
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir(dir.join("media"))
        .expect("media 目录应存在")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(handles, on_disk, "导出物里的句柄集合必须与 media/ 下的文件集合相等");
    assert!(handles.contains("deadbeef.png"), "桩那条媒体句柄应在其中: {handles:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 取字节失败（5xx）必须让导出**响亮失败**，而不是静默少下载几个媒体还退 0。
///
/// 此前任何错误都被当作「不是可取句柄」跳过（注释口径却只说 404），于是瞬时 5xx 会让交付物少
/// 媒体、而退出码仍是 0 —— 没有任何信号。现在只有 404 才跳过，其余上抛、该会话进 skipped。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_media_fails_loudly_when_bytes_fetch_errors() {
    let base = spawn_stub().await;
    let dir = std::env::temp_dir().join(format!("weflow-e2e-mediaboom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (code, stdout, stderr) = run(
        &base,
        &[
            "export", "--out", dir.to_str().unwrap(), "--format", "jsonl", "--with-media",
            "--session", "boom-talker",
        ],
    );
    assert_eq!(code, 1, "取字节 5xx 应让该会话失败并以非零码结束；stdout: {stdout} stderr: {stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("跳过"), "应当说明会话被跳过（而不是静默退 0）: {combined}");
    // 点名是哪个会话：只断言「有跳过字样」的话，跳错会话／跳了别的也会过。
    assert!(combined.contains("boom-talker"), "跳过说明要点名会话: {combined}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 404 与 5xx **必须区别对待**：404 =「这个句柄本来就取不到」→ 跳过、整轮仍成功；
/// 5xx =「这次没拿到」→ 上抛、该会话进 skipped。只测一侧，就会漏掉「把 404 也改成失败」的实现。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_media_skips_an_unfetchable_handle_without_failing() {
    let base = spawn_stub().await;
    let dir = std::env::temp_dir().join(format!("weflow-e2e-mediagone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (code, stdout, stderr) = run(
        &base,
        &[
            "export", "--out", dir.to_str().unwrap(), "--format", "jsonl", "--with-media",
            "--session", "gone-talker",
        ],
    );
    assert_eq!(code, 0, "404 是「不可取句柄」，不该让整轮失败；stdout: {stdout} stderr: {stderr}");
    assert!(!dir.join("media").join("gone.png").exists(), "取不到的句柄不该留下字节");
    let jsonl = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("应有 jsonl");
    let body = std::fs::read_to_string(&jsonl).unwrap();
    assert!(!body.contains("gone.png"), "没有字节的句柄不该写进导出物: {body}");
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
    // 未截断的那条路径也要钉：只测「截断」的反面（把 truncated／hasMore 写死为真的实现）会漏。
    let fetched_json: Value = serde_json::from_str(
        fetched["result"]["content"][0]["text"].as_str().unwrap_or_default(),
    )
    .expect("工具输出应是 JSON");
    assert_eq!(fetched_json["truncated"], json!(false), "单条消息不该被截断: {fetched_json}");
    assert_eq!(fetched_json["hasMore"], json!(false), "单条消息不该报还有更多: {fetched_json}");

    child.kill().ok();
    let _ = child.wait();
}

/// 起一个 `mcp` 子进程、握手、调一次工具，返回 `result` 对象后杀掉进程。
async fn mcp_call(base: &str, tool: &str, args: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_weflow-server"))
        .arg("mcp")
        .env("WEFLOW_BASE_URL", base)
        .env("WEFLOW_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn weflow-server mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "e2e", "version": "0.1.0"}},
    }));
    let _ = read_until(&mut out, 1);
    send(&mut stdin, json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(&mut stdin, json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    }));
    let resp = read_until(&mut out, 2);
    child.kill().ok();
    let _ = child.wait();
    resp["result"].clone()
}

/// 预算截断时必须让调用方知道「还有」，否则 agent 会把截断当成「取完了」并静默丢掉消息。
///
/// 钉的是 `hasMore` 与 `truncated` 的一致性：只置 `truncated` 而 `hasMore=false`，按 hasMore 判停的
/// 调用方会停在一个不完整的结果上。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_truncation_reports_has_more_so_the_caller_does_not_stop() {
    let base = spawn_stub().await;
    let r = mcp_call(&base, "search_messages", json!({"talker": TALKER, "keyword": "big"})).await;
    assert_ne!(r["isError"], json!(true), "不该是工具级错误: {r}");
    let text = r["content"][0]["text"].as_str().unwrap_or_default();
    let v: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("工具输出应是 JSON: {e} / {text}"));
    assert_eq!(v["truncated"], json!(true), "60 条 2KB 消息应超预算: {v}");
    assert_eq!(v["hasMore"], json!(true), "截断时 hasMore 必须为真: {v}");
    assert!(
        v["nextOffset"].as_u64().is_some_and(|n| n > 0),
        "必须给一个**能回传**的续拉游标（此前的 nextCursor 无处回传）: {v}"
    );
    assert!(v["count"].as_u64().unwrap_or(99) < 60, "count 应少于总条数: {v}");
}
/// 起一个 mcp 子进程后，从工具结果里取出结构化 JSON。
fn tool_json(r: &Value) -> Value {
    let text = r["content"][0]["text"].as_str().unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|e| panic!("工具输出应是 JSON: {e} / {text}"))
}

/// `search_messages` 必须给出一个**能回传**的游标，且按它续拉要真的前进。
///
/// 此前它返回 ChatLab 的 `nextCursor`，而这个工具的参数与 SDK 的查询结构都没有 cursor 字段 ——
/// 那个游标无处可传，第 2 页永远取不到（等于分页承诺不可兑现）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_search_pagination_actually_advances() {
    let base = spawn_stub().await;
    let first =
        tool_json(&mcp_call(&base, "search_messages", json!({"talker": TALKER, "keyword": "big"})).await);
    assert_eq!(first["truncated"], json!(true), "首屏应被预算截断: {first}");
    let next = first["nextOffset"].as_u64().expect("必须给可回传的 nextOffset");
    assert!(next > 0, "nextOffset 必须前进: {first}");
    let second = tool_json(
        &mcp_call(&base, "search_messages", json!({"talker": TALKER, "keyword": "big", "offset": next}))
            .await,
    );
    let a = first["messages"][0]["platformMessageId"].as_str().unwrap_or_default().to_string();
    let b = second["messages"][0]["platformMessageId"].as_str().unwrap_or_default().to_string();
    assert_ne!(a, b, "按 nextOffset 续拉必须前进，而不是取回同一页");
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
