//! 一致性套件的**执行入口**：造夹具 → 起真实服务 → 跑 `flow-contract` 的 runner。
//!
//! 为什么是「测试」而不是独立二进制：它需要的一切（造库、拿 token、等 ready）都已经在
//! 测试树里；独立二进制则要把它们搬到库里（那属于库边界的工作）。CI 里仍是一个**独立步骤**：
//!
//!   cargo test --locked --test conformance_runner -- --ignored --nocapture
//!
//! 需要环境变量 `FLOW_CONTRACT_DIR` 指向契约仓库的检出（CI 会按 pin 的 tag clone）。
//! 本文件的套件测试带 `#[ignore]`：日常 `cargo test` 不跑它（这是设计，不是漏洞），CI 用
//! `--ignored` 显式跑。**跑它而缺环境变量时是 panic，不是静默通过**——判据不在 `contract_dir()`
//! （它只返回 Option），在 `conformance_suite_passes` 体内的 `let Some(contract) = … else { panic! }`。
//! 之所以失败开放：CI 那步就是带着 `--ignored` 来的，若缺环境也返回 Ok，报表上会出现
//! 「一致性套件绿了」而实际一个端点都没验过。
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
use axum::response::IntoResponse;
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
                // 游标相对追加（opaqueCursor 用例）：向 cursor 表达的入库进度
                // **之后**追加一条消息，回显新行的 platformMessageId（WeFlow 侧
                // 即 server_id 字符串——测试夹具里它由递增序号保证唯一）。本仓库
                // 的消息表按 (create_time, sort_seq, local_id) 排序、增量按 local_id
                // 推进，故游标之后 = local_id 更大即可（同一秒或更晚都可）。
                if v["action"].as_str() == Some("harness.append_message_at_cursor") {
                    let key = weflow_server::keystore::parse_db_key(&key_hex).unwrap();
                    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    common::append_group_message_unique(&storage, &key.0, 8100 + seq);
                    let acct = state.accounts.lock().values().next().cloned();
                    if let Some(a) = acct {
                        let sync = a.sync.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || sync.lock().poll_once()).await;
                    }
                    // 回显新行的 id（用例经 save 取回作为 appended_id 基准）。
                    // append_group_message_unique 的 server_id 由序号派生，这里
                    // 直接以同源序号构造。
                    let id = format!("{}", 8_400_000_000_000_000_000i64 + 8100 + seq);
                    return axum::Json(serde_json::json!({ "success": true, "id": id }))
                        .into_response();
                }
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
                axum::http::StatusCode::OK.into_response()
            }
        }),
    )
}
fn cfg_for(dir: &std::path::Path) -> weflow_server::config::Config {
    weflow_server::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        log: "warn".into(),
        log_format: "text".into(),
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

/// `conformance.pin` 与夹具的 `contractVersion` **必须同改**。
///
/// 为什么需要它：runner 只比对**夹具**与契约仓库的 `VERSION`，它不读 git tag ——
/// 于是「只改 pin、忘了夹具」在本地不会红（要到 CI 运行时才以 exit 2 拒绝），
/// 反过来「只改夹具、忘了 pin」更隐蔽：本地跑本地 clone 的契约目录照样通过，
/// 而 CI clone 的是旧 tag。两处一起改因此必须是可执行的，而不是靠记性。
#[test]
fn pinned_contract_version_matches_the_fixture() {
    let pin = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/conformance.pin"))
        .expect("conformance.pin 必须存在");
    let pin = pin.trim().trim_start_matches('v');
    assert_eq!(
        pin, CONTRACT_VERSION,
        "conformance.pin 与夹具的 contractVersion 不一致：升 pin 时两处必须同改"
    );
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

/// 夹具声明的契约版本。**必须与 `conformance.pin` 同改** —— 见下面的
/// `pinned_contract_version_matches_the_fixture`。
const CONTRACT_VERSION: &str = "0.10.0";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "由 CI 的一致性步骤显式运行：需要 FLOW_CONTRACT_DIR"]
async fn conformance_suite_passes() {
    // 缺目录**不是**「跳过」，是失败：这条测试唯一的存在理由就是跑那套用例，
    // 而它此前在环境缺失时静默 return —— 于是 CI 里少配一个变量就会让整套门禁
    // 变成「绿着什么都没验」。
    let Some(contract) = contract_dir() else {
        panic!(
            "[conformance] 未设置 FLOW_CONTRACT_DIR（或其中没有 runner/run.py）：             一致性套件是本仓库的门禁之一，缺环境必须失败而不是静默通过"
        );
    };
    let dir = common::tmp_dir("conformance");
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);
    // 同秒多条：Pull 面按**整秒组**分页，没有这样的行，相关用例就没有可验证的对象。
    // 时间戳要**落在**夹具已有的消息（…100–103）与 harness 追加之用（…200）**之间**：
    // 用更晚的会让水位线越过 200，于是 harness 追加的那条被判成「旧的」，
    // `poll_once` 返回 0、SSE 上永远等不到 `message.new`（我第一版就是这么写的）。
    common::append_same_second_burst(&storage, &key.0, 4, 1_700_000_150);
    // The existing group image gets a real matching local source before indexing.
    {
        use md5::Digest;
        let bytes = b"\x89PNG\r\n\x1a\ncontract image";
        let digest = format!("{:x}", md5::Md5::digest(bytes));
        let room = format!("{:x}", md5::Md5::digest(common::FAKE_GROUP.as_bytes()));
        let source = dir.join("msg/attach").join(&room).join("fixture/Img");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(format!("{digest}.dat")), bytes).unwrap();
        let writer = common::wx_conn(&storage.join("message/message_0.db"), &key.0, false);
        writer.execute(&format!("UPDATE \"Msg_{room}\" SET message_content=?1 WHERE local_type=3"),
            [format!("<msg><img md5=\"{digest}\"/></msg>")]).unwrap();
    }


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
    // 鉴权探测：契约的 `authProbed` 是「传输名 → 状态码」，由 harness 探测后填入。
    // 这不是断言，是**告知** —— 用例据此判断哪些通道可用。
    //
    // **只探两条**：凭据写法不同（`authorization` 要 `Bearer ` 前缀，查询参数是裸 token），
    // 而 `X-Api-Key`、`?token=` 与 POST body 已不是通道 —— 探它们只会得到 401，而 401 与
    // 「通道还在但没带对凭据」在探测结果里无法区分。
    let mut auth_probed = serde_json::Map::new();
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
        ("access_token", plain_get("access_token")),
    ];
    for (name, r) in probes {
        let (n, s) = probe(client.clone(), name, r).await;
        auth_probed.insert(n, json!(s));
    }
    // 导出根预置一条可取的媒体文件（mediaId 承诺用例）：名字由内容摘要派生
    //（32 hex），与 mediaId 同值——按句柄取字节即取这个文件。
    let exported_dir = dir.join("media").join(common::FAKE_GROUP).join("voices");
    std::fs::create_dir_all(&exported_dir).unwrap();
    std::fs::write(
        exported_dir.join("00112233445566778899aabbccddeeff.silk"),
        b"#!SILK_V3-fixture-bytes",
    )
    .unwrap();

    let fx = json!({
        "contractVersion": CONTRACT_VERSION,
        "platform": "wechat",
        "generatedBy": "weflow-server tests/conformance_runner.rs",
        // 夹具必须声明 schema 里要求的每个端点（缺一个即 setup 失败，而不是静默跳过）。
        "endpoints": {
            "accounts": "/api/v1/accounts",
            "contacts": "/api/v1/contacts",
            "health": "/health",
            "messages": "/api/v1/messages",
            "messages_chatlab": "/chatlab/messages",
            // 媒体字节端点（mediaById 能力的落点，契约用例
            // nails-media-id-fetch-succeeds 按句柄取字节）。
            "media": "/api/v1/media/{id}",
            "media_export": "/api/v1/media/export",
            "harness": "/__harness",
            "group-members": "/api/v1/group-members",
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
            // harness 保证句柄此刻可取的媒体（mediaId 承诺用例的槽位）。
            // harness 保证句柄此刻可取的媒体（mediaId 承诺用例的槽位）：
            // 夹具预置一条语音导出。**id 是导出文件名**（本面按名解析的形状：
            // <md5>.<ext>），与文件系统里预置的文件同名。
            "exported_media": { "id": "00112233445566778899aabbccddeeff.silk", "expect": {} },
            "targeted_media": {"id":common::FAKE_GROUP, "expect":{
                "platformMessageId": 8_200_000_000_000_000_002i64.to_string(),
                "sender":"wxid_member_b", "timestamp":1_700_000_102i64}},
        },
        "capabilities": {
            "mediaById": true,
            "mediaExportByMessage": true,
            // 不透明进度游标（契约 0.9.0）：sync.nextCursor + cursor 参数已实现，
            // 排空后同秒晚入库的行可经游标续拉到达。
            "opaqueCursor": true,
            "sns": true,
            "memberCount": true,
            // Pull 形状的发现面（`GET /chatlab/sessions`）已实现：置 true 让相关用例实跑。
            // （此处曾写着「尚未实现、置 false 跳过」——能力翻真后注释没跟上；
            // 注释与取值矛盾比没有注释更误导，故改写。）
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
        // 有跳过即失败：case 级 skip 此前不影响退出码，「夹具少声明一个端点」
        // 会让用例静默变成不跑而 CI 全绿（见 runner 的同名开关）。
        .arg("--fail-on-skip")
        .current_dir(&contract)
        .output()
        .expect("python 必须可用（提交路径本来就依赖它）");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}\n{stderr}");

    serve.abort();
    assert!(out.status.success(), "一致性套件未通过（见上面的报告）");
}

/// 游标续拉的**行标识单调性**回归：入库序与业务时间序在乱序行上分叉时，
/// cursor 模式的过滤、排序、页推进与游标编码必须全部只用行标识。
///
/// 修复前会踩中其中一段：
///
/// - **重放**（「单页排空 + 续拉」段）：排空按时间序一页交付全部行时，页末行的 localId
///   不是已交付集合的最大值（回溯入库的行时间早、行标识大，排序在最前、
///   页末也轮不到它）。旧代码编码页末行 → 回传续拉用 `localId > 页末值`
///   过滤 → 该行被**再次交付**。
/// - **空页进度重置**（「空页游标语义」段）：空页旧代码编码 0 —— 那是
///   「从头拉取」，带 `end` 过滤的续拉把全历史重放一遍。
/// - **页推进**（「同秒组续接」段）：cursor 模式按时间组扩展页边界，同秒行 + 乱序行的组合
///   会把页边界切在行标识序中间，「已交付上界」不严格。
///
/// 健全域说明：混合链（时间模式多页 → 游标续接）在乱序行存在时**单点游标
/// 本质上不可健全**——页 1 已交付大行标识的回溯行，小行标识的未交付行永远
/// 无法经游标到达。游标链只在「采纳游标前已交付集合是行标识前缀」时健全，
/// 单页全量排空保证这一点（契约用例 cursor-boundary-append-reachable 正是
/// 此形态）。本测试钉住健全域内的全部判别性场景。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实索引链路回归：随一致性步骤显式运行（cargo test --test conformance_runner -- --ignored）"]
async fn cursor_continuation_is_monotonic_when_row_order_diverges_from_time_order() {
    let dir = common::tmp_dir("cursor-ooo");
    let key = keystore::parse_db_key(common::FAKE_KEY_HEX).unwrap();
    let storage = common::build_wechat_account(&dir, &key.0);

    // 回溯入库在**注册之前**：业务时间早于夹具全部群行（…100–103 → …050），
    // 行标识更大（AUTOINCREMENT 排在最后）。注册后的全量索引把它一并收进来。
    // （注册后追加不可行：增量扫描按时间水位线推进，回溯行会永远落在
    // 水位线之后被跳过，索引里根本没有它——第一版编排就是这么失败的。）
    common::append_group_message_backdated(&storage, &key.0, 990, 1_700_000_050);

    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(server::AppState::new(
        cfg_for(&dir),
        TOKEN.to_string(),
        shutdown_tx,
    ));
    let app = server::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = app.clone();
    tokio::spawn(async move {
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

    let pull_url = format!("/chatlab/sessions/{}/messages", common::FAKE_GROUP);
    let backdated = format!("{}", 8_450_000_000_000_000_000i64 + 990);

    // 段一：单页排空 + 一次续拉——无重放。
    // limit 放大到能一页装下全部 5 行（4 行常规 + 回溯行）。时间序把回溯行
    // 排在最前、页末轮不到它——旧代码编码页末行，续拉把回溯行再交付一次。
    let v = json_of(&client, req("GET", &format!("{pull_url}?limit=5000"))).await;
    let first_page: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(v["sync"]["hasMore"], json!(false), "必须单页排空");
    assert!(first_page.contains(&backdated), "回溯行必须在排空页里");
    assert_eq!(first_page.len(), 5, "排空页必须交付全部 5 行");

    let c0 = v["sync"]["nextCursor"].as_str().unwrap();
    let v = json_of(
        &client,
        req("GET", &format!("{pull_url}?limit=5000&cursor={c0}")),
    )
    .await;
    let got: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert!(
        got.is_empty(),
        "排空后按游标续拉必须为空（重放判据），实际交付：{got:?}"
    );

    // 段二：空页游标语义。
    //  - 带 cursor 的空页：进度保持（游标值不变，不回退为从头拉取）；
    //  - 无 cursor 的空 time-filtered 页：游标编码「end 界内已确立的最大行
    //    标识」——不是 0（从头拉取）。周期轮询的客户端（end=now）采纳空页
    //    游标再丢掉时间过滤，必须不重放全历史。
    let v = json_of(
        &client,
        req("GET", &format!("{pull_url}?limit=5000&cursor={c0}&end=1700000000")),
    )
    .await;
    let got: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert!(got.is_empty(), "end 过滤必须让本页为空");
    let c1 = v["sync"]["nextCursor"].as_str().unwrap();
    assert_eq!(c1, c0, "带 cursor 的空页必须保持进度（游标不变）");

    // 无 cursor 的空 time-filtered 页：窗口 (1700000060, 1700000099] 内
    // 无行，但「ts ≤ end 的行都已确立」——游标必须编码回溯行（ts=…050、行标识
    // 最大），而不是 0（从头拉取）。周期轮询的客户端（end=now）采纳空页游标
    // 再丢掉时间过滤，必须不重放全历史。
    let v = json_of(
        &client,
        req("GET", &format!("{pull_url}?limit=5000&since=1700000060&end=1700000099")),
    )
    .await;
    let got: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert!(got.is_empty(), "end 过滤必须让本页为空");
    let c_empty = v["sync"]["nextCursor"].as_str().unwrap();
    // 游标形状 `v2.<nonce>.<账号>.<注册代次>.<数据源世代>.<会话>.<localId>`，
    // 整段 URL-safe base64（无填充）：标准字母表的 `+` 会在 query 里被解成空格。
    use base64::Engine as _;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(c_empty)
            .unwrap(),
    )
    .unwrap();
    let lid: i64 = decoded
        .split('.')
        .next_back()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    assert_ne!(
        lid, 0,
        "空页游标不得编码行标识 0（从头拉取）：{decoded}"
    );
    // 采纳空页游标续拉（不带时间过滤）：不得重放全历史。
    let v = json_of(
        &client,
        req("GET", &format!("{pull_url}?limit=5000&cursor={c_empty}")),
    )
    .await;
    let got: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert!(
        got.is_empty(),
        "空页游标（end 界内已确立进度）续拉不得重放全历史，实际交付：{got:?}"
    );

    // 段三：排空后追加（同秒、行标识更大）→ 游标续拉可达。
    // 基准游标是段一的排空游标 c0（全量排空线，全部行在链内）；段二的空页
    // 游标属于「end=99 的时间过滤线」，它确立的进度只覆盖 ts ≤ 99 的行，
    // 拿它接无过滤链会跳过时间更晚的常规行——那不是缺陷，是线语义。
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    common::append_group_message_unique(&storage, &key.0, 8_100 + seq);
    let acct = state.accounts.lock().values().next().cloned();
    if let Some(a) = acct {
        let sync = a.sync.clone();
        let _ = tokio::task::spawn_blocking(move || sync.lock().poll_once()).await;
    }
    let v = json_of(
        &client,
        req("GET", &format!("{pull_url}?limit=5000&cursor={c0}")),
    )
    .await;
    let got: Vec<String> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        got,
        vec![format!("{}", 8_400_000_000_000_000_000i64 + 8_100 + seq)],
        "排空后追加的行必须经排空游标可达（且只交付它）"
    );

    // 段四：同秒组 + 游标续接——页推进按行标识组。从段三的游标线继续（cursor
    // 模式全链），limit=2 强制分页：验证「页边界按行标识组扩展」而不是按
    // 时间组，且链上无重放无跳行。
    common::append_same_second_burst(&storage, &key.0, 2, 1_700_000_200);
    let acct = state.accounts.lock().values().next().cloned();
    if let Some(a) = acct {
        let sync = a.sync.clone();
        let _ = tokio::task::spawn_blocking(move || sync.lock().poll_once()).await;
    }
    let mut cursor: Option<String> = Some(c0.to_string());
    let mut pages: Vec<Vec<String>> = Vec::new();
    loop {
        let q = match &cursor {
            Some(c) => format!("?limit=2&cursor={c}"),
            None => unreachable!("cursor 模式排空链游标必须恒在场"),
        };
        let v = json_of(&client, req("GET", &format!("{pull_url}{q}"))).await;
        let page_ids: Vec<String> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["platformMessageId"].as_str().unwrap().to_string())
            .collect();
        let more = v["sync"]["hasMore"].as_bool().unwrap();
        pages.push(page_ids);
        cursor = Some(
            v["sync"]["nextCursor"]
                .as_str()
                .expect("cursor 模式排空链上 nextCursor 必须在场")
                .to_string(),
        );
        if !more {
            break;
        }
    }
    let all: Vec<String> = pages.iter().flatten().cloned().collect();
    let mut sorted = all.clone();
    sorted.sort();
    let mut expected = [
        // 段四从排空游标 c0 续接：本次循环交付的是 local_id 更大的三行——段三的
        // 追加行（同秒 …200）与 burst 两行（同秒 …200）。limit=2 下 cursor 模式
        // 页推进按行标识（每行即一组）：同秒三行被切成 2+1 两页——游标线的
        // 「已交付上界」优先于时间分组，这与时间模式「页不跨秒」是两套规则。
        8_400_000_000_000_000_000i64 + 8_100 + seq,
        8_300_000_000_000_000_000i64,
        8_300_000_000_000_000_001i64,
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>();
    expected.sort();
    // 页推进判别：修复前页推进按时间组——同秒三行整体一页（页数=1）；修复后
    // 按行标识推进，limit=2 切成两页（2+1）。
    assert_eq!(
        pages.len(),
        2,
        "cursor 模式页推进必须按行标识（limit=2 把同秒三行切成两页），实际页数：{}",
        pages.len()
    );
    assert_eq!(sorted, expected, "游标链（limit=2）交付集合必须与剩余行逐条相等");
    let mut dedup = all;
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), sorted.len(), "游标链不得重复交付（重放判据）");
}
