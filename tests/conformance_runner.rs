//! 一致性套件的**执行入口**：造夹具 → 起真实服务 → 跑 `flow-contract` 的 runner。
//!
//! 为什么是「测试」而不是独立二进制：它需要的一切（造库、拿 token、等 ready）都已经在
//! 测试树里；独立二进制则要把它们搬到库里（那属于库边界的工作）。CI 里仍是一个**独立步骤**：
//!
//!   cargo test --locked --test conformance_runner -- --ignored --nocapture
//!
//! 需要环境变量 `FLOW_CONTRACT_DIR` 指向契约仓库的检出（CI 会按 pin 的 tag clone）。
//! 没有它时本测试**跳过并通过**，这样在没检出契约仓库的开发机上 `cargo test` 依然全绿 ——
//! 而 CI 上它一定会跑（那里一定有）。
//!
//! ## 为什么自己构造 `AppState` 而不是跑服务端二进制
//!
//! 服务端二进制的 token 存在 OS 凭据库里；CI 上没有凭据库，它会回退成随机 token 并**只写进
//! 日志** —— runner 拿不到它。测试自己构造状态就能自己指定 token，同时仍然走**真实注册路径**
//! （零账号启动 → `POST /api/v1/accounts` → 等 ready），不手工往注册表里塞东西。

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request};
use serde_json::{json, Value};
use tower::ServiceExt;

use weflow_server::keystore;
use weflow_server::server;

const TOKEN: &str = "conformance-runner-token-0123456789";

/// harness 每次追加消息用一个递增序号，保证 `server_id` 唯一（见 `append_message` 的注释）。
static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn contract_dir() -> Option<std::path::PathBuf> {
    let d = std::env::var("FLOW_CONTRACT_DIR").ok()?;
    let p = std::path::PathBuf::from(d);
    p.join("runner/run.py").exists().then_some(p)
}

/// 测试专用的 harness 控制端点，**不进入产品路由**。
///
/// 契约里三条用例要「让服务端发生一件事」（追加一条消息、注销账号）才能验证 —— 那是**动作**，
/// 不是请求，只能由 harness 提供。放在测试里而不是产品里：产品多一个能改状态的未鉴权端点，
/// 是给所有人开的门；而这里只有本测试能碰到它（它 merge 在测试自己构造的 Router 上）。
fn harness_router(
    account_root: std::path::PathBuf,
    storage: std::path::PathBuf,
    key_hex: String,
    state: Arc<server::AppState>,
) -> axum::Router {
    axum::Router::new().route(
        "/__harness",
        axum::routing::post(move |body: String| {
            let account_root = account_root.clone();
            let storage = storage.clone();
            let state = state.clone();
            async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                match v["action"].as_str().unwrap_or("") {
                    "harness.append_message" => {
                        // **前提恢复**：用例之间必须互相独立，而 `sse-deregister-replay` 会
                        // 注销账号 —— 它字母序在前，于是本条用例在「没有账号」的环境里跑，
                        // 追加的消息没人索引，SSE 上永远等不到。这里按需重新注册。
                        //
                        // （契约层面「用例必须独立」是一条性质；在 harness 里恢复前提是最小
                        // 的修法 —— 产品行为没有被绕过，本用例要验的东西照旧。）
                        let needs_register = state.accounts.lock().is_empty();
                        if needs_register {
                            let body = weflow_server::server::handlers::accounts::AccountBody {
                                wxid: Some(common::FAKE_WXID.to_string()),
                                key: Some(key_hex.clone()),
                                db_path: Some(account_root.to_string_lossy().into_owned()),
                                ..Default::default()
                            };
                            let _ = server::start_account(state.clone(), body).await;
                            for _ in 0..120 {
                                let ready = state
                                    .accounts
                                    .lock()
                                    .values()
                                    .next()
                                    .map(|a| a.status().is_ready())
                                    .unwrap_or(false);
                                if ready {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            }
                            println!("[harness] 账号不在（前面的用例注销了它），已重新注册");
                        }
                        // **不能用 `common::append_group_message`**：它的 `server_id` 是硬编码的，
                        // 而本 harness 会被多条用例调用（`cursor-incremental-only-new` 与
                        // `sse-notification-shape`）—— 两次调用就会产生两条同一个
                        // `platformMessageId` 的消息，`nails-platform-message-id-string`
                        // 如实报「页内重复」。判据是对的，错在夹具。
                        let key = keystore::parse_db_key(&key_hex).unwrap();
                        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        common::append_group_message_unique(&storage, &key.0, seq);
                        // 追加后立刻同步一次：Watcher 也会发现，但等它会让用例变成计时敏感的。
                        //
                        // **必须是 `poll_once` 而不是 `full_sync`**：广播事件的是前者；用后者
                        // 索引会更新、库里也有新行，但 SSE 上永远等不到 `message.new` ——
                        // 表现为「在 N 秒内没有收到事件」（我第一版就是这么写的）。
                        let acct = state.accounts.lock().values().next().cloned();
                        if let Some(a) = acct {
                            let sync = a.sync.clone();
                            let _ = tokio::task::spawn_blocking(move || sync.lock().poll_once()).await;
                        }
                    }
                    "harness.deregister" => {
                        // 注销会停掉 watcher 并清空索引，正是那条用例要的状态。
                        let qq = state
                            .accounts
                            .lock()
                            .keys()
                            .next()
                            .cloned()
                            .unwrap_or_default();
                        let _ = tokio::task::spawn_blocking(move || {
                            server::deregister_account(&state, &qq, false)
                        })
                        .await;
                    }
                    _ => {}
                }
                axum::http::StatusCode::OK
            }
        }),
    )
}
fn cfg_for(dir: &std::path::Path) -> weflow_server::config::Config {
    weflow_server::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        log: "warn".into(),
        watch_debounce_ms: 20,
        watch_fallback_ms: 0,
        media_export_dir: dir.join("media"),
        base_url: None,
        show_token: false,
        data_dir: dir.join("data"),
    }
}

fn req(method: &str, uri: &str) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    r.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    );
    r
}

/// 发一次鉴权探测，返回 (传输名, 状态码)。
async fn probe(app: axum::Router, name: &str, r: Request<Body>) -> (String, u16) {
    let resp = app.oneshot(r).await.unwrap();
    (name.to_string(), resp.status().as_u16())
}

async fn json_of(app: &axum::Router, r: Request<Body>) -> Value {
    let resp = app.clone().oneshot(r).await.unwrap();
    let b = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
    serde_json::from_slice(&b).unwrap_or(Value::Null)
}

/// 等后台索引构建到 ready。超时即失败 —— 夹具很小，慢到这个程度说明是真卡住了。
async fn wait_ready(app: &axum::Router) -> bool {
    for _ in 0..120 {
        let v = json_of(app, req("GET", "/api/v1/accounts")).await;
        let st = v["accounts"][0]["state"].as_str().unwrap_or("");
        if st == "ready" {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "由 CI 的一致性步骤显式运行：需要 FLOW_CONTRACT_DIR"]
async fn conformance_suite_passes() {
    let Some(contract) = contract_dir() else {
        println!("[conformance] 未设置 FLOW_CONTRACT_DIR（或其中没有 runner/run.py），跳过");
        return;
    };
    let dir = common::tmp_dir("conformance");
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);
    // 同秒多条：Pull 面按**整秒组**分页，没有这样的行，相关用例就没有可验证的对象。
    // 时间戳要**落在**夹具已有的消息（…100–103）与 harness 追加之用（…200）**之间**：
    // 用更晚的会让水位线越过 200，于是 harness 追加的那条被判成「旧的」，
    // `poll_once` 返回 0、SSE 上永远等不到 `message.new`（我第一版就是这么写的）。
    common::append_same_second_burst(&storage, &key.0, 4, 1_700_000_150);

    // 零账号启动，随后走真实注册路径。
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        cfg_for(&dir),
        TOKEN.to_string(),
        shutdown_tx,
    ));
    // 产品路由 + 测试专用 harness（后者只在这次测试的进程里存在）。
    let app = server::build_router(state.clone()).merge(harness_router(
        dir.clone(),
        storage.clone(),
        common::FAKE_KEY_HEX.to_string(),
        state.clone(),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = app.clone();
    let serve = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let body = json!({
        "wxid": common::FAKE_WXID,
        "key": common::FAKE_KEY_HEX,
        "db_path": dir.to_string_lossy(),
    });
    let mut r = req("POST", "/api/v1/accounts");
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    *r.body_mut() = Body::from(body.to_string());
    let reg = json_of(&client, r).await;
    assert_eq!(reg["success"], json!(true), "注册失败：{reg}");
    assert!(wait_ready(&client).await, "索引没能在 30 秒内到 ready");

    // 写夹具文件并交给 runner。
    // 五通道鉴权探测：契约的 `authProbed` 是「传输名 → 状态码」，由 harness 探测后填入。
    // 这不是断言，是**告知** —— 用例据此判断哪些通道可用。
    let mut auth_probed = serde_json::Map::new();
    // 五种传输各自的**凭据写法不同**：`authorization` 要 `Bearer ` 前缀，`x-api-key` 要裸
    // token，两个查询参数与 body 都是裸 token。第一版把两种 header 都写成 `Bearer `，
    // 于是 `x-api-key` 探出 401、断言当场报「这些传输方式没有返回 200：[x-api-key]」。
    let plain_get = |q: &str| {
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/sessions?{q}={TOKEN}"))
            .body(Body::empty())
            .unwrap()
    };
    // `HeaderMap::insert` 要求 `'static` 的键，所以这里收 `&'static str` 而不是 `&str`。
    let with_header = |h: &'static str, v: String| {
        let mut r = Request::builder()
            .method("GET")
            .uri("/api/v1/sessions")
            .body(Body::empty())
            .unwrap();
        r.headers_mut().insert(h, v.parse().unwrap());
        r
    };
    let probes = [
        ("bearer", with_header("authorization", format!("Bearer {TOKEN}"))),
        ("x-api-key", with_header("x-api-key", TOKEN.to_string())),
        ("access_token", plain_get("access_token")),
        ("token", plain_get("token")),
        (
            "body",
            Request::builder()
                .method("POST")
                .uri("/api/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "access_token": TOKEN }).to_string()))
                .unwrap(),
        ),
    ];
    for (name, r) in probes {
        let (n, s) = probe(client.clone(), name, r).await;
        auth_probed.insert(n, json!(s));
    }
    let fx = json!({
        "contractVersion": "0.2.2",
        "platform": "wechat",
        "generatedBy": "weflow-server tests/conformance_runner.rs",
        // schema 要求七项齐全（用例只用到其中两个，但夹具的完整性由 schema 校验）。
        "endpoints": {
            "accounts": "/api/v1/accounts",
            "contacts": "/api/v1/contacts",
            "health": "/health",
            "messages": "/api/v1/messages",
            "harness": "/__harness",
            "pull": "/chatlab/sessions/{id}/messages",
            "push": "/chatlab/push/messages",
            "sessions": "/chatlab/sessions",
        },
        "authProbed": auth_probed,
        "slots": {
            "group_with_messages": { "id": common::FAKE_GROUP, "expect": { "messages": 9 } },
            "private_with_messages": { "id": common::FAKE_FRIEND, "expect": { "messages": 4 } },
            "same_second_burst": { "id": common::FAKE_GROUP, "expect": { "same_second": 4 } },
            "unknown": { "id": "wxid_does_not_exist_000000000", "expect": {} },
        },
        "capabilities": {
            "mediaById": true,
            "sns": true,
            "memberCount": true,
            // Pull 形状的发现面（规范里的 `GET {baseUrl}/sessions`）尚未实现 —— 它是计划里
            // 「ChatLab 适配」那一步的内容。置 false 让相关用例**跳过而不是失败**：契约的
            // 能力机制就是为这种「存在性差异」准备的。实现后翻成 true 即启用。
            "pullDiscovery": true,
            // SSE 通知面**是存在的**且符合契约：`GET {baseUrl}/push/messages` ＋
            // `message.new`/`message.revoke`/`sync` 三种事件都通过套件断言。
            //
            // （先前这里写的是 false，理由是「规范要求通知帧只带元信息、不带消息体，而本仓库
            // 推完整消息」—— 那是我**读规范读出来的推断**，套件实测推翻：`event_notification_shape`
            // 对现行载荷是通过的。推断不该当作结论。）
            "pullNotification": true,
            "roles": false,
            "sse": true,
            "authProbe": true,
        },
    });
    let fx_path = dir.join("fixture.json");
    std::fs::write(&fx_path, serde_json::to_string_pretty(&fx).unwrap()).unwrap();

    // `FLOW_CONTRACT_CASE` 透传给 runner 的 `--case`：把一条用例单独拉出来查。
    // 一整套跑下来时，报错本身往往不足以定位问题出在哪条路径上。
    let case_filter: Vec<String> = std::env::var("FLOW_CONTRACT_CASE")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let out = std::process::Command::new("python")
        .arg(contract.join("runner/run.py"))
        .arg("--base-url")
        .arg(format!("http://{addr}"))
        .arg("--cases")
        .arg(contract.join("cases"))
        .arg("--fixture")
        .arg(&fx_path)
        .arg("--token")
        .arg(TOKEN)
        .args(case_filter.iter().flat_map(|c| ["--case", c.as_str()]))
        .current_dir(&contract)
        .output()
        .expect("python 必须可用（提交路径本来就依赖它）");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}\n{stderr}");

    serve.abort();
    assert!(out.status.success(), "一致性套件未通过（见上面的报告）");
}
