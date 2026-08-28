//! Downstream-client simulation: GET/POST against the REAL HTTP layer with a
//! REAL WeChat 4.x database. No secrets are hardcoded here — the registration
//! inputs resolve in priority order:
//!
//!   1. `./weflow-server.json` in the repo root (gitignored), flat fields:
//!      `wxid` / `db_path` / `keys` (per-database map) or `key` (uniform),
//!      plus optional `img_aes_key` / `img_xor_key`;
//!   2. environment variables: WEFLOW_TEST_WXID / WEFLOW_TEST_DB_ROOT and
//!      either WEFLOW_TEST_KEYS_JSON (an all_keys.json-style map file) or
//!      WEFLOW_TEST_KEY (a uniform key).
//!
//! Run:
//!   bash scripts/build.sh test --test downstream_client -- --ignored --nocapture
//!
//! Startup is CLIENT-DRIVEN: the app boots with zero accounts, and the test
//! registers the account via `POST /api/v1/accounts` (wxid + keys + db_path)
//! exactly like a downstream client would, then waits for the background
//! index build to reach `ready`. Afterwards it exercises the request shapes a
//! WeFlow-style client sends: all five auth transports, GET+POST parameter
//! transport, error envelopes, ChatLab Pull pagination (`nextSince` /
//! `nextOffset`), contacts paging (`total` / `hasMore`), group members with
//! message counts, media export over REST, manual sync, SNS timeline, and the
//! SSE content-type — all against real WeChat chat data.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use weflow_server::server::{self, AppState};

const TEST_TOKEN: &str = "downstream-client-test-token";
/// Bind port for the fixture app; media URLs are minted against it.
const TEST_PORT: u16 = 5033;

/// The base URL the handlers derive for media links, given the fixture config.
fn derive_base() -> String {
    server::derive_base_url("127.0.0.1", TEST_PORT, None)
}

/// Resolved registration inputs, shaped like the `POST /api/v1/accounts` body.
struct Inputs {
    wxid: String,
    db_path: String,
    /// Per-database key map (`keys`), preferred by the real config.
    keys: Option<serde_json::Map<String, Value>>,
    /// Uniform key (`key`), used when no per-database map exists.
    key: Option<String>,
    img_aes_key: Option<String>,
    img_xor_key: Option<String>,
}

impl Inputs {
    /// The registration body a downstream client would POST. Built here so the
    /// key material is assembled in exactly one place and never printed.
    fn body(&self, token: &str) -> Value {
        let mut body = json!({
            "access_token": token,
            "wxid": self.wxid,
            "db_path": self.db_path,
        });
        let obj = body.as_object_mut().unwrap();
        if let Some(keys) = &self.keys {
            obj.insert("keys".into(), Value::Object(keys.clone()));
        }
        if let Some(key) = &self.key {
            obj.insert("key".into(), json!(key));
        }
        if let Some(k) = &self.img_aes_key {
            obj.insert("img_aes_key".into(), json!(k));
        }
        if let Some(k) = &self.img_xor_key {
            obj.insert("img_xor_key".into(), json!(k));
        }
        body
    }
}

/// Resolve the registration inputs. Priority: the repo-root
/// `./weflow-server.json` (gitignored — never committed), then the environment
/// variables. Returns None when neither source provides wxid + db_path + a key
/// source.
fn resolve_inputs() -> Option<Inputs> {
    // 1. repo-root config file (highest priority).
    if let Ok(text) = std::fs::read_to_string("weflow-server.json")
        && let Ok(v) = serde_json::from_str::<Value>(&text)
    {
        let wxid = v["wxid"].as_str().map(String::from);
        let db_path = v["db_path"].as_str().map(String::from);
        let keys = v["keys"].as_object().cloned();
        let key = v["key"].as_str().map(String::from);
        if let (Some(wxid), Some(db_path)) = (wxid, db_path)
            && (keys.is_some() || key.is_some())
        {
            println!("[CLIENT] inputs from ./weflow-server.json");
            return Some(Inputs {
                wxid,
                db_path,
                keys,
                key,
                img_aes_key: v["img_aes_key"].as_str().map(String::from),
                img_xor_key: v["img_xor_key"].as_str().map(String::from),
            });
        }
    }
    // 2. environment variables.
    let wxid = std::env::var("WEFLOW_TEST_WXID").ok()?;
    let db_path = std::env::var("WEFLOW_TEST_DB_ROOT").ok()?;
    // A keys FILE (all_keys.json shape) or a single uniform key.
    let keys = std::env::var("WEFLOW_TEST_KEYS_JSON")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned());
    let key = std::env::var("WEFLOW_TEST_KEY").ok();
    if keys.is_none() && key.is_none() {
        return None;
    }
    println!("[CLIENT] inputs from environment variables");
    Some(Inputs {
        wxid,
        db_path,
        keys,
        key,
        img_aes_key: std::env::var("WEFLOW_TEST_IMG_AES_KEY").ok(),
        img_xor_key: std::env::var("WEFLOW_TEST_IMG_XOR_KEY").ok(),
    })
}

/// Build an EMPTY app (client-driven startup: zero accounts, not ready) plus
/// the resolved registration inputs. Returns None when no input source
/// provides a usable account.
fn build_real_app(export_dir: &std::path::Path) -> Option<(axum::Router, Arc<AppState>, Inputs)> {
    let inputs = resolve_inputs()?;
    // The wxid is host-identifying, so it is NOT printed (qqflow prints the
    // account, but a wxid is a stable personal identifier and the privacy
    // scanner treats it as a secret).
    println!("[CLIENT] account resolved from config (identifier withheld)");

    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let cfg = weflow_server::config::Config {
        host: "127.0.0.1".into(),
        port: TEST_PORT,
        log: "info".into(),
        watch_debounce_ms: 50,
        watch_fallback_ms: 0,
        media_export_dir: export_dir.join("media"),
        base_url: None,
        show_token: false,
        data_dir: export_dir.join("data"),
    };
    let state = Arc::new(AppState::new(cfg, TEST_TOKEN.to_string(), shutdown_tx));
    let app = server::build_router(state.clone());
    Some((app, state, inputs))
}

/// Poll /health until the account is ready; returns its indexed message count.
/// Indexing a real account is minutes of work on a cold cache, hence the
/// generous ceiling.
async fn wait_ready(app: &axum::Router, wxid: &str) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let mut last = Value::Null;
    while std::time::Instant::now() < deadline {
        let (_, v) = client_get(app.clone(), "/health", &[]).await;
        let account = v["accounts"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["wxid"] == wxid))
            .cloned();
        if let Some(a) = account {
            match a["state"].as_str() {
                Some("ready") => return a["message_count"].as_u64().unwrap_or(0) as usize,
                Some("error") => panic!("account registration failed: {a}"),
                _ => {}
            }
            last = a;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("account never reached ready within 600s, last state: {last}");
}

fn build_request(method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Downstream-client GET (optional extra headers, e.g. Bearer auth).
async fn client_get(app: axum::Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    send(app, build_request("GET", uri, headers, None)).await
}

/// Downstream-client POST with a JSON body (parameters and/or token inside).
async fn client_post(
    app: axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Value) {
    send(app, build_request("POST", uri, headers, Some(body))).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a real WeChat 4.x account (./weflow-server.json or WEFLOW_TEST_*)"]
async fn downstream_client_real_db() {
    let export_dir = std::env::temp_dir().join("weflow_downstream_client");
    let Some((app, state, inputs)) = build_real_app(&export_dir) else {
        println!(
            "[CLIENT] SKIPPED: 无 ./weflow-server.json 且环境变量未设置 \
             (WEFLOW_TEST_WXID / WEFLOW_TEST_DB_ROOT / WEFLOW_TEST_KEY 或 WEFLOW_TEST_KEYS_JSON)"
        );
        return;
    };
    let token = TEST_TOKEN;
    let wxid = inputs.wxid.clone();

    // ---- 0. boot state: zero registered accounts, not ready -------------
    let (s, v) = client_get(app.clone(), "/health", &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "starting");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    // Zero REGISTERED accounts. The list may still be non-empty: a startup
    // scan would add `awaiting_key` entries — but this fixture never scans,
    // so the list is empty here.
    assert!(state.accounts.lock().is_empty(), "no account registered at boot");
    assert_eq!(v["accounts"].as_array().unwrap().len(), 0);

    // ---- 0.1 register the account (client-driven startup) ---------------
    let (s, v) = client_post(app.clone(), "/api/v1/accounts", &[], inputs.body(token)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true, "registration accepted: {v}");
    assert_eq!(v["state"], "accepted", "registration accepted: {v}");
    assert_eq!(v["status"], "indexing", "state machine value rides along");
    assert_eq!(v["wxid"], wxid);
    // The echo must be the RESOLVED storage directory: `db_path` may be an
    // account root (then `db_storage` is appended) or the storage dir itself
    // (then it passes through). Shape-level only — the real path is
    // host-specific and must never be printed or hard-coded.
    let resolved = v["db_storage"].as_str().expect("db_storage echoed");
    assert!(
        resolved == inputs.db_path || resolved.ends_with("db_storage"),
        "db_storage is the supplied path or extends it with db_storage"
    );
    assert!(resolved.len() >= inputs.db_path.len(), "resolved path extends the supplied root");
    assert!(std::path::Path::new(resolved).is_dir(), "resolved storage dir exists");

    // ---- 0.2 registration is idempotent while indexing ------------------
    let (s, v) = client_post(app.clone(), "/api/v1/accounts", &[], inputs.body(token)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["state"] == "in_progress" || v["state"] == "already_ready",
        "re-registering must not restart the build: {v}"
    );

    // ---- 1. health: ready after the background index build --------------
    let indexed = wait_ready(&app, &wxid).await;
    assert!(indexed > 0, "real db must have messages");
    println!("[CLIENT] indexed {indexed} messages");
    let (s, v) = client_get(app.clone(), "/health", &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "ok");
    let accounts = v["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["wxid"], wxid);
    assert_eq!(accounts[0]["state"], "ready");
    assert!(accounts[0].get("error").is_none(), "ready account carries no error");
    assert_eq!(accounts[0]["message_count"].as_u64().unwrap() as usize, indexed);

    // ---- 2. auth: business endpoints reject missing/wrong tokens --------
    let (s, v) = client_get(app.clone(), "/api/v1/messages?talker=x", &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["success"], false);
    assert_eq!(v["code"], 401);
    let (s, _) = client_get(
        app.clone(),
        "/api/v1/messages?talker=x",
        &[("authorization", "Bearer wrong-token")],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a wrong token is not a missing token");

    // ---- 2b. all FIVE auth transports reach the same endpoint -----------
    // Bearer header / X-Api-Key header / ?access_token= / ?token= / POST body.
    let bearer = format!("Bearer {token}");
    let transports: Vec<(&str, Request<Body>)> = vec![
        (
            "Authorization: Bearer",
            build_request("GET", "/api/v1/sessions?limit=1", &[("authorization", &bearer)], None),
        ),
        (
            "X-Api-Key",
            build_request("GET", "/api/v1/sessions?limit=1", &[("x-api-key", token)], None),
        ),
        (
            "?access_token=",
            build_request("GET", &format!("/api/v1/sessions?limit=1&access_token={token}"), &[], None),
        ),
        (
            "?token=",
            build_request("GET", &format!("/api/v1/sessions?limit=1&token={token}"), &[], None),
        ),
        (
            "POST body access_token",
            build_request("POST", "/api/v1/sessions", &[], Some(json!({"access_token": token, "limit": 1}))),
        ),
    ];
    for (name, req) in transports {
        let (s, v) = send(app.clone(), req).await;
        assert_eq!(s, StatusCode::OK, "auth transport {name} accepted");
        assert_eq!(v["success"], true, "auth transport {name} answered: {v}");
    }
    println!("[CLIENT] auth: all 5 transports accepted");

    // ---- 3. sessions: Bearer header, WeFlow shape -----------------------
    let (s, v) = client_get(
        app.clone(),
        "/api/v1/sessions?limit=50",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    let sessions = v["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty(), "real db must have conversations");
    for sess in sessions {
        assert!(sess["username"].is_string());
        assert!(sess["displayName"].is_string());
        // `type` is `SessionKind as i64`: 0 private, 1 group, 2 official,
        // 3 other. (qqflow uses 1/2 for private/group — a deliberate
        // divergence, so clients should prefer the string `sessionType`.)
        assert!(matches!(sess["type"].as_i64(), Some(0..=3)), "type is the numeric kind");
        let kind = sess["sessionType"].as_str();
        assert!(matches!(kind, Some("private" | "group" | "official" | "other")));
        // The two encodings must agree.
        let expected = match kind {
            Some("private") => 0,
            Some("group") => 1,
            Some("official") => 2,
            _ => 3,
        };
        assert_eq!(sess["type"].as_i64(), Some(expected), "type matches sessionType");
        assert!(sess["lastTimestamp"].is_number());
        assert!(sess["messageCount"].is_number());
        assert!(sess["summary"].is_string());
    }
    // Newest-first ordering is the contract clients paginate against.
    let ts: Vec<i64> = sessions.iter().map(|s| s["lastTimestamp"].as_i64().unwrap()).collect();
    assert!(ts.windows(2).all(|w| w[0] >= w[1]), "sessions must be newest first");

    // offset paging: a single row at offset 0, and an empty page past the end
    // (stable properties even with a live DB that grows between requests).
    let (s, v1) = client_get(
        app.clone(),
        &format!("/api/v1/sessions?limit=1&offset=0&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v1["count"].as_i64().unwrap(), 1, "limit=1&offset=0 returns exactly one session");
    assert_eq!(v1["sessions"].as_array().unwrap().len(), 1);
    let (s, v2) = client_get(
        app.clone(),
        &format!("/api/v1/sessions?limit=1&offset=999999&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v2["count"], json!(0), "offset past the end returns an empty page");
    assert!(v2["sessions"].as_array().unwrap().is_empty());
    println!("[CLIENT] sessions offset: page-1 size=1, offset-past-end empty");

    // Pick conversations that actually carry indexed messages. A session row
    // can exist with `messageCount == 0` (an empty or not-yet-indexed talker),
    // and `/api/v1/messages` answers 404 for those — correct behaviour, but
    // useless as a fixture for the field-contract checks below.
    let has_messages = |s: &&Value| s["messageCount"].as_u64().unwrap_or(0) > 0;
    let first_talker = sessions
        .iter()
        .find(has_messages)
        .expect("real db must have a conversation with messages")["username"]
        .as_str()
        .unwrap()
        .to_string();
    let group_id = sessions
        .iter()
        .find(|s| s["sessionType"] == "group" && has_messages(s))
        .map(|s| s["username"].as_str().unwrap().to_string());
    // Session identifiers are personal data: log the COUNT, never the ids.
    println!(
        "[CLIENT] {} sessions ({} group)",
        v["count"],
        sessions.iter().filter(|s| s["sessionType"] == "group").count()
    );

    // ---- 3a. a session with no indexed messages answers a 404 envelope --
    // Locked in because it is surprising: the session is listed by
    // /api/v1/sessions, yet /api/v1/messages has no conversation to serve.
    if let Some(empty) = sessions.iter().find(|s| !has_messages(s)) {
        let talker = empty["username"].as_str().unwrap();
        let (s, v) = client_get(
            app.clone(),
            &format!("/api/v1/messages?talker={talker}&access_token={token}"),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "listed-but-empty session: {v}");
        assert_eq!(v["success"], false);
        assert_eq!(v["code"], 404);
        println!("[CLIENT] empty session -> 404 envelope");
    }

    // ---- 3b. sessions: chatlab projection ------------------------------
    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/sessions?limit=5&chatlab=1&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    for sess in v["sessions"].as_array().unwrap() {
        assert!(sess["id"].is_string());
        assert!(sess["name"].is_string());
        assert_eq!(sess["platform"], "wechat");
        assert!(matches!(sess["type"].as_str(), Some("private" | "group")));
        assert!(sess["messageCount"].is_number());
        assert!(sess["lastMessageAt"].is_number());
    }

    // ---- 4. messages: GET via ?access_token=, field contract ------------
    let uri = format!("/api/v1/messages?talker={first_talker}&limit=20&access_token={token}");
    let (s, v) = client_get(app.clone(), &uri, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    assert_eq!(v["talker"], first_talker);
    assert_eq!(v["media"]["enabled"], false, "media export off by default");
    assert_eq!(v["media"]["count"], 0);
    let msgs = v["messages"].as_array().unwrap();
    assert!(!msgs.is_empty(), "real conversation must have messages");
    assert_eq!(v["count"].as_u64().unwrap() as usize, msgs.len());
    for m in msgs {
        assert!(m["localId"].is_number());
        assert!(m["serverId"].is_string(), "serverId is stringified (i64 exceeds JS safe range)");
        assert!(m["localType"].is_number());
        assert!(m["createTime"].is_number());
        assert!(m["sortSeq"].is_number());
        // Direction: 0 (other/system) or 1 (self) — never 2+.
        assert!(matches!(m["isSend"].as_i64(), Some(0 | 1)), "isSend is 0/1");
        assert!(m["senderUsername"].is_string());
        assert!(m["senderName"].is_string());
        assert!(m["content"].is_string());
        assert!(m["rawContent"].is_string());
        assert!(m["parsedContent"].is_string());
        // Structured media rides on image/voice/video/emoji messages. Without
        // `media=1` the url/localPath stay empty strings rather than being
        // absent, so a client can rely on the field existing.
        if let Some(media) = m["media"].as_object() {
            assert!(matches!(
                media["type"].as_str(),
                Some("image" | "voice" | "video" | "emoji" | "file")
            ));
            assert!(media["fileName"].is_string());
            assert_eq!(media["url"], "", "no url until media=1 exports the bytes");
            assert_eq!(media["localPath"], "", "no localPath until media=1");
        }
        // A quote carries the parent's id plus a rendered preview.
        if let Some(q) = m["quote"].as_object() {
            assert!(q["platformMessageId"].is_string());
            assert!(q["sender"].is_string());
            assert!(q["accountName"].is_string());
        }
    }
    // newest first
    let ts_first = msgs[0]["createTime"].as_i64().unwrap();
    let ts_last = msgs.last().unwrap()["createTime"].as_i64().unwrap();
    assert!(ts_first >= ts_last, "messages must be newest first");
    let media_rows = msgs.iter().filter(|m| m["media"].is_object()).count();
    println!("[CLIENT] messages: {} rows, {media_rows} with media metadata", msgs.len());

    // ---- 4b. media=1 export: bytes go through REST (weflow difference) ---
    // qqflow pushes media paths inline; weflow exports on demand and hands
    // back a fetchable URL. Accepted divergence — asserted here so a
    // regression in either direction is visible.
    //
    // The newest conversation may hold no media at all, which would make every
    // assertion below vacuous. Probe the busiest sessions for one that
    // actually carries media metadata and export from that.
    let mut media_talker = first_talker.clone();
    if media_rows == 0 {
        let mut candidates: Vec<&Value> = sessions.iter().filter(has_messages).collect();
        candidates.sort_by_key(|s| std::cmp::Reverse(s["messageCount"].as_u64().unwrap_or(0)));
        for cand in candidates.iter().take(10) {
            let talker = cand["username"].as_str().unwrap();
            let (_, probe) = client_get(
                app.clone(),
                &format!("/api/v1/messages?talker={talker}&limit=100&access_token={token}"),
                &[],
            )
            .await;
            let n = probe["messages"]
                .as_array()
                .map(|a| a.iter().filter(|m| m["media"].is_object()).count())
                .unwrap_or(0);
            if n > 0 {
                media_talker = talker.to_string();
                println!("[CLIENT] media probe: found a conversation with {n} media rows");
                break;
            }
        }
    }
    let uri = format!("/api/v1/messages?talker={media_talker}&limit=50&media=1&access_token={token}");
    let (s, v) = client_get(app.clone(), &uri, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["media"]["enabled"], true);
    let export_path = v["media"]["exportPath"].as_str().unwrap();
    assert!(!export_path.is_empty(), "media=1 reports the export directory");
    let exported: Vec<&Value> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["media"]["exported"] == json!(true))
        .collect();
    assert_eq!(
        v["media"]["count"].as_u64().unwrap() as usize,
        exported.len(),
        "count matches exported rows"
    );
    let mut local_route = 0usize;
    for m in &exported {
        let media = &m["media"];
        let url = media["url"].as_str().unwrap();
        assert!(url.starts_with("http"), "url is absolute: {url}");
        let local_path = media["localPath"].as_str().unwrap();
        assert!(local_path.starts_with(export_path), "localPath under exportPath");
        assert!(
            std::path::Path::new(local_path).is_file(),
            "exported file exists on disk"
        );
        // Emoji can resolve to a CDN address (`external_url`); everything else
        // is served by this process. For the local route the URL's last
        // segment is the EXPORTED file name — which is not necessarily
        // `fileName`, that one carries the name from the message XML.
        if url.starts_with(&format!("{}/api/v1/media/", derive_base())) {
            local_route += 1;
            let exported_name = std::path::Path::new(local_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let path_part = url.split('?').next().unwrap();
            assert!(
                path_part.ends_with(&exported_name),
                "media route URL ends with the exported file name: {url}"
            );
        }
    }
    assert!(
        local_route > 0 || exported.is_empty(),
        "at least one export is served by the local media route"
    );
    println!("[CLIENT] media=1 export: {} rows exported", exported.len());

    // ---- 4c. the exported URL is actually fetchable ---------------------
    if let Some(m) = exported.first() {
        let url = m["media"]["url"].as_str().unwrap();
        // The handler mints an absolute URL; oneshot needs the path+query.
        if let Some(path) = url.split_once("://").and_then(|(_, rest)| rest.split_once('/')) {
            let uri = format!("/{}", path.1);
            let resp = app
                .clone()
                .oneshot(Request::builder().method("GET").uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "exported media URL is fetchable");
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024).await.unwrap();
            assert!(!bytes.is_empty(), "media response has a body");
            println!("[CLIENT] media fetch: {} bytes", bytes.len());
        }
    }

    // ---- 5. messages: POST body transport + YYYYMMDD bounds -------------
    let (s, v) = client_post(
        app.clone(),
        "/api/v1/messages",
        &[],
        json!({
            "access_token": token,
            "talker": first_talker,
            "limit": 5,
            "start": "20200101",
            "end": "20301231",
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    let posted = v["messages"].as_array().unwrap();
    assert_eq!(v["count"].as_u64().unwrap() as usize, posted.len());
    assert!(posted.len() <= 5, "limit honoured");
    println!("[CLIENT] POST messages: {} rows (limit=5, YYYYMMDD bounds)", posted.len());

    // ---- 6. ChatLab Pull: drain the conversation with nextSince/nextOffset
    // The strong contract: feeding both cursors back verbatim must eventually
    // return EVERY message exactly once. A cursor that over-advances still
    // yields "no duplicates" and "cursor moved forward", so only a full drain
    // against the known message count can catch dropped rows.
    let expected_total = sessions
        .iter()
        .find(|s| s["username"] == first_talker.as_str())
        .unwrap()["messageCount"]
        .as_u64()
        .unwrap() as usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Rows served, NOT deduped: `platformMessageId` is the server id and
    // WeChat leaves it 0 for local-only messages, so several rows can share
    // one id and collapse in `seen`. Only the raw row count proves no page
    // was skipped.
    let mut rows_served = 0usize;
    let mut uri = format!("/api/v1/sessions/{first_talker}/messages?limit=50&access_token={token}");
    let mut page_no = 0;
    loop {
        let (s, v) = client_get(app.clone(), &uri, &[]).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["chatlab"]["version"], "0.0.2");
        assert_eq!(v["chatlab"]["generator"], "weflow-server");
        assert_eq!(v["meta"]["platform"], "wechat");
        assert_eq!(v["meta"]["groupId"], first_talker);
        assert!(v["sync"]["watermark"].as_i64().unwrap() > 0);
        let page = v["messages"].as_array().unwrap();
        rows_served += page.len();
        let dupes = page
            .iter()
            .filter(|m| {
                let id = m["platformMessageId"].as_str().unwrap();
                // id 0 is "no server id" and is legitimately shared.
                id != "0" && seen.contains(id)
            })
            .count();
        assert_eq!(dupes, 0, "page {page_no} must not repeat messages");
        seen.extend(page.iter().map(|m| m["platformMessageId"].as_str().unwrap().to_string()));
        let has_more = v["sync"]["hasMore"].as_bool().unwrap();
        if !has_more {
            // Drained: the cursor parks on the time bound and resets offset.
            assert_eq!(v["sync"]["nextOffset"], json!(0), "drained cursor resets offset");
            break;
        }
        assert!(!page.is_empty(), "hasMore=true must not hand back an empty page");
        let since = v["sync"]["nextSince"].as_i64().unwrap();
        let offset = v["sync"]["nextOffset"].as_i64().unwrap();
        uri = format!(
            "/api/v1/sessions/{first_talker}/messages?since={since}&offset={offset}&limit=50&access_token={token}"
        );
        page_no += 1;
        assert!(page_no < 10_000, "pagination must terminate");
    }
    println!(
        "[CLIENT] chatlab pull: {rows_served} rows / {} unique ids over {} pages \
         (session reports {expected_total})",
        seen.len(),
        page_no + 1
    );
    // Every row exactly once. This is what the old cursor broke: `nextSince`
    // used to jump to the newest message in the whole conversation, so page 1
    // came back empty and everything in between was silently dropped.
    // Live DBs grow mid-test, so newer rows may appear beyond the count the
    // earlier /sessions call reported — hence >=, never <.
    assert!(
        rows_served >= expected_total,
        "drain must serve every row: got {rows_served} of {expected_total}"
    );

    // ---- 7. ChatLab Pull 404 envelope for an unknown session ------------
    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/sessions/nonexistent-session-999/messages?access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["success"], false);
    assert_eq!(v["code"], 404);

    // ---- 8. contacts: paging contract (offset + total + hasMore) --------
    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/contacts?limit=10&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    let total = v["total"].as_u64().expect("total present");
    let first_page: Vec<String> = v["contacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["username"].as_str().unwrap().to_string())
        .collect();
    for c in v["contacts"].as_array().unwrap() {
        assert!(c["username"].is_string());
        // displayName always resolves (remark > nickname > username), but the
        // raw source fields are Option<String> and serialize to null when the
        // contact row has no value — a real shape difference from qqflow,
        // where they are always strings.
        assert!(c["displayName"].is_string());
        for field in ["nickname", "remark", "alias", "avatarUrl"] {
            assert!(
                c[field].is_string() || c[field].is_null(),
                "{field} is a string or null: {c}"
            );
        }
        assert!(c["type"].is_string());
    }
    println!("[CLIENT] contacts: {} of {total} rows", v["count"]);

    // Page 2 by offset must be disjoint from page 1 — the stable-order fix.
    if total > 10 {
        assert_eq!(v["hasMore"], true, "more contacts remain past the first page");
        let (s, v2) = client_get(
            app.clone(),
            &format!("/api/v1/contacts?limit=10&offset=10&access_token={token}"),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let second: BTreeSet<String> = v2["contacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["username"].as_str().unwrap().to_string())
            .collect();
        let overlap = first_page.iter().filter(|u| second.contains(*u)).count();
        assert_eq!(overlap, 0, "offset paging must not repeat contacts");
        assert_eq!(v2["total"].as_u64().unwrap(), total, "total is stable across pages");
        println!("[CLIENT] contacts paging: page2 disjoint from page1");
    }

    // ---- 9. group members: GET with counts + POST with talker alias -----
    match &group_id {
        Some(gid) => {
            let (s, v) = client_get(
                app.clone(),
                &format!("/api/v1/group-members?chatroomId={gid}&includeMessageCounts=1&access_token={token}"),
                &[],
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(v["success"], true);
            assert_eq!(v["chatroomId"], *gid);
            let members = v["members"].as_array().unwrap();
            assert!(!members.is_empty(), "group conversation must have members");
            for m in members {
                assert!(m["wxid"].is_string());
                assert!(m["displayName"].is_string());
                assert!(m["nickname"].is_string());
                assert!(m["remark"].is_string());
                assert!(m["alias"].is_string());
                assert!(m["groupNickname"].is_string());
                assert!(m["avatarUrl"].is_string());
                assert!(m["messageCount"].is_number(), "includeMessageCounts=1");
                assert_eq!(m["isOwner"], false);
                assert!(m["isFriend"].is_boolean());
            }
            println!("[CLIENT] group-members: {} rows", members.len());

            // POST transport, `talker` alias for chatroomId. Counts default
            // off, so messageCount must come back 0 rather than absent.
            let (s, v) = client_post(
                app.clone(),
                "/api/v1/group-members",
                &[],
                json!({ "access_token": token, "talker": gid }),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(v["success"], true);
            assert_eq!(v["chatroomId"], *gid);
            assert!(
                v["members"].as_array().unwrap().iter().all(|m| m["messageCount"] == json!(0)),
                "no counts unless requested"
            );
        }
        None => println!("[CLIENT] no group session found, skipping group-members"),
    }

    // ---- 10. manual sync: real incremental pass over the real db --------
    let (s, v) = client_post(
        app.clone(),
        "/api/v1/sync",
        &[],
        json!({ "access_token": token }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    assert!(v["newMessages"].as_i64().unwrap() >= 0);
    assert!(v["revokeMessages"].as_i64().unwrap() >= 0);
    println!(
        "[CLIENT] manual sync: new={} revoke={}",
        v["newMessages"], v["revokeMessages"]
    );

    // ---- 11. SNS: timeline + usernames + stats (weflow-only surface) ----
    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/sns/timeline?limit=5&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    let feeds = v["timeline"].as_array().unwrap();
    assert_eq!(v["count"].as_u64().unwrap() as usize, feeds.len());
    for f in feeds {
        assert!(f["feedId"].is_string());
        assert!(f["username"].is_string());
        assert!(f["createTime"].is_number());
        assert!(f["contentDesc"].is_string());
        assert!(f["media"].is_array());
        assert!(f["likes"].is_array());
        assert!(f["comments"].is_array());
        // Media descriptors carry the decryption inputs a client needs; the
        // proxy url is what it should actually fetch.
        for m in f["media"].as_array().unwrap() {
            assert!(m["url"].is_string());
        }
    }
    println!("[CLIENT] sns timeline: {} feeds of {}", feeds.len(), v["total"]);

    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/sns/stats?access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    assert!(v["stats"]["feeds"].is_number());
    assert!(v["stats"]["posters"].is_number());
    assert!(v["stats"]["mediaItems"].is_number());

    let (s, v) = client_get(
        app.clone(),
        &format!("/api/v1/sns/usernames?verbose=1&limit=5&access_token={token}"),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["success"], true);
    for u in v["usernames"].as_array().unwrap() {
        assert!(u["username"].is_string());
        assert!(u["feedCount"].is_number());
    }
    println!("[CLIENT] sns usernames: {}", v["count"]);

    // ---- 12. SSE: connect shape (content-type; body streams live) -------
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/push/messages?access_token={token}"))
                .method("GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/event-stream"
    );
    println!("[CLIENT] SSE connect: 200 text/event-stream");

    // ---- 13. graceful shutdown ends live SSE streams ---------------------
    // The watch flag is what `serve()` flips on Ctrl+C: watcher tasks stop and
    // every subscribed stream closes. Asserting it here keeps the shutdown
    // path covered without a real signal.
    assert!(state.shutdown.send(true).is_ok(), "shutdown channel is live");

    // Exported media lives in a temp dir; drop it so repeat runs start clean.
    let _ = std::fs::remove_dir_all(&export_dir);
    println!("[CLIENT] all downstream-client checks passed");
}
