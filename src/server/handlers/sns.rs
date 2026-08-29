//! SNS (朋友圈) endpoints — WeFlow-aligned surface:
//! - GET /api/v1/sns/timeline
//! - GET /api/v1/sns/usernames
//! - GET /api/v1/sns/stats
//!
//! Data source: `sns.db:SnsTimeLine` read through the live-pool index (read-only).
//! Moments media is referenced by CDN url (thumb/full) — WeChat 4.x does not
//! cache moments media on disk by default, so no local export applies.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::handlers::{extract_params, ready_account, require_auth};
use crate::server::AppState;

fn sns_feed_json(
    store: &crate::store::Store,
    f: &crate::store::SnsFeed,
    base_url: &str,
    token: &str,
) -> serde_json::Value {
    let proxy = |raw: &str| {
        if raw.is_empty() {
            serde_json::Value::String(String::new())
        } else {
            serde_json::Value::String(format!(
                "{base_url}/api/v1/sns/media/proxy?url={}&access_token={token}",
                urlencoding_encode(raw)
            ))
        }
    };
    let media: Vec<serde_json::Value> = f
        .media
        .iter()
        .map(|m| {
            let mut e = json!({
                "url": proxy(&m.url),
                "thumb": proxy(m.thumb.as_deref().unwrap_or("")),
                "md5": m.md5,
                "token": m.token,
                "key": m.key,
                "encIdx": m.enc_idx,
                "rawUrl": m.url,
                "rawThumb": m.thumb,
                "resolvedUrl": m.url,
                "resolvedThumbUrl": m.thumb,
            });
            if let Some(o) = e.as_object_mut() {
                o.insert("proxyUrl".into(), o["url"].clone());
                o.insert("proxyThumbUrl".into(), o["thumb"].clone());
                o.insert("width".into(), json!(m.width));
                o.insert("height".into(), json!(m.height));
            }
            e
        })
        .collect();
    let contact = store.contacts.get(&f.user_name);
    json!({
        "tid": f.feed_id.parse::<i64>().unwrap_or(0),
        "id": f.object_id,
        "username": f.user_name,
        "nickname": if f.nickname.is_empty() { store.session_display(&f.user_name) } else { f.nickname.clone() },
        "avatarUrl": contact.and_then(|c| c.avatar_url.clone()).unwrap_or_default(),
        "createTime": f.create_time,
        "contentDesc": f.content_desc,
        "type": f.content_type,
        "media": media,
        "likes": f.likes.iter().map(|l| json!({
            "username": l.username,
            "nickname": l.nickname,
            "createTime": l.create_time,
        })).collect::<Vec<_>>(),
        "comments": f.comments.iter().map(|cm| json!({
            "username": cm.username,
            "nickname": cm.nickname,
            "createTime": cm.create_time,
            "content": cm.content,
        })).collect::<Vec<_>>(),
        "rawXml": f.raw_xml,
        "location": { "latitude": f.latitude, "longitude": f.longitude },
        // retained extras for convenience
        "feedId": f.feed_id,
        "displayName": store.session_display(&f.user_name),
        "kind": f.kind,
        "commentCount": f.comment_count,
    })
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// GET /api/v1/sns/timeline?username=&limit=&offset=&start=&end=
pub async fn timeline(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;
    let username = params.get("username").filter(|s| !s.is_empty()).cloned();
    let limit = crate::server::parse_limit(&params, "limit", 50, 500);
    let offset = crate::server::parse_offset(&params, "offset");
    let start = params.get("start").and_then(|s| crate::server::parse_time_bound(s));
    let end = params.get("end").and_then(|s| crate::server::parse_time_bound(s));

    let store = account.store.read();
    // newest-first already; apply filters then window
    let matched: Vec<&crate::store::SnsFeed> = store
        .sns_feeds
        .iter()
        .filter(|f| {
            username.as_ref().is_none_or(|u| &f.user_name == u)
                && start.is_none_or(|s| f.create_time >= s)
                && end.is_none_or(|e| f.create_time <= e)
        })
        .collect();
    let total = matched.len();
    let base_url = &state.base_url;
    let slice: Vec<serde_json::Value> = matched
        .iter()
        .skip(offset)
        .take(limit)
        .map(|f| sns_feed_json(&store, f, base_url, &state.token))
        .collect();
    Ok(Json(json!({
        "success": true,
        "count": slice.len(),
        "total": total,
        "hasMore": offset + slice.len() < total,
        "timeline": slice,
    })))
}

/// GET /api/v1/sns/usernames — distinct posters with display names.
pub async fn usernames(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;

    let store = account.store.read();
    use std::collections::BTreeMap;
    let mut by_user: BTreeMap<String, (i64, usize)> = BTreeMap::new();
    for f in &store.sns_feeds {
        let e = by_user.entry(f.user_name.clone()).or_insert((0, 0));
        e.0 = e.0.max(f.create_time);
        e.1 += 1;
    }
    let verbose = crate::server::flex_bool(&params, "verbose");
    let mut rows: Vec<(String, i64, usize)> = by_user
        .into_iter()
        .map(|(username, (last, n))| (username, last, n))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let count = rows.len();
    // WeFlow contract: `usernames` is an array of display-name strings.
    // verbose=1 opts back into the richer objects (ours extension).
    let usernames: Vec<serde_json::Value> = if verbose {
        rows.iter()
            .map(|(u, last, n)| {
                json!({
                    "username": u,
                    "displayName": store.session_display(u),
                    "feedCount": n,
                    "lastPostTime": last,
                })
            })
            .collect()
    } else {
        rows.iter()
            .map(|(u, _, _)| {
                let d = store.session_display(u);
                serde_json::Value::String(if d.is_empty() { u.clone() } else { d })
            })
            .collect()
    };
    Ok(Json(json!({ "success": true, "count": count, "usernames": usernames })))
}

/// GET /api/v1/sns/stats — aggregate counts (privacy-safe summary).
pub async fn stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;

    let store = account.store.read();
    let feeds = &store.sns_feeds;
    let total = feeds.len();
    let by_kind: std::collections::BTreeMap<&str, usize> =
        feeds.iter().fold(Default::default(), |mut m, f| {
            *m.entry(f.kind).or_insert(0) += 1;
            m
        });
    let media_count: usize = feeds.iter().map(|f| f.media.len()).sum();
    let comment_count: usize = feeds.iter().map(|f| f.comment_count).sum();
    let posters = {
        let mut s: Vec<String> = feeds.iter().map(|f| f.user_name.clone()).collect();
        s.sort();
        s.dedup();
        s.len()
    };
    let time_range = match (feeds.last(), feeds.first()) {
        (Some(a), Some(b)) => json!({"from": a.create_time, "to": b.create_time}),
        _ => json!(null),
    };
    Ok(Json(json!({
        "success": true,
        "stats": {
            "feeds": total,
            "posters": posters,
            "byKind": by_kind,
            "mediaItems": media_count,
            "comments": comment_count,
            "timeRange": time_range,
        },
    })))
}

// ---------------------------------------------------------------------------
// sns/export — serialize the cached timeline into a downloadable document.
// ---------------------------------------------------------------------------

fn year_of(ts: i64) -> String {
    // days since epoch -> civil year (proleptic Gregorian, good enough here)
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    (yoe + era * 400).to_string()
}

pub(crate) fn aggregate(feeds: &[crate::store::SnsFeed]) -> serde_json::Value {
    let by_kind: std::collections::BTreeMap<&str, usize> =
        feeds.iter().fold(Default::default(), |mut m, f| {
            *m.entry(f.kind).or_insert(0) += 1;
            m
        });
    let mut by_year: std::collections::BTreeMap<String, usize> = Default::default();
    let mut media_count = 0usize;
    let mut comment_count = 0usize;
    for f in feeds {
        media_count += f.media.len();
        comment_count += f.comment_count;
        if f.create_time > 0 {
            *by_year.entry(year_of(f.create_time)).or_insert(0) += 1;
        }
    }
    let posters = {
        let mut s: Vec<String> = feeds.iter().map(|f| f.user_name.clone()).collect();
        s.sort();
        s.dedup();
        s.len()
    };
    let time_range = match (feeds.last(), feeds.first()) {
        (Some(a), Some(b)) => json!({"from": a.create_time, "to": b.create_time}),
        _ => json!(null),
    };
    json!({
        "feeds": feeds.len(),
        "posters": posters,
        "byKind": by_kind,
        "byYear": by_year,
        "mediaItems": media_count,
        "comments": comment_count,
        "timeRange": time_range,
    })
}

/// GET/POST /api/v1/sns/export?username=&format=json|html
///
/// Serialize the locally cached timeline into a standalone document under
/// `<data_dir>/exports/`. Read-only w.r.t. WeChat data.
pub async fn export(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;
    let username = params.get("username").filter(|s| !s.is_empty()).cloned();
    let format = params
        .get("format")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "json".into());
    if !matches!(format.as_str(), "json" | "html") {
        return Err(ApiError::bad_request("format must be json or html"));
    }

    let store = account.store.read();
    let feeds: Vec<&crate::store::SnsFeed> = store
        .sns_feeds
        .iter()
        .filter(|f| username.as_ref().is_none_or(|u| &f.user_name == u))
        .collect();
    let feed_owned: Vec<crate::store::SnsFeed> = feeds.iter().map(|f| (*f).clone()).collect();
    let stats = aggregate(&feed_owned);
    let base_url = &state.base_url;
    let entries: Vec<serde_json::Value> = feeds
        .iter()
        .map(|f| sns_feed_json(&store, f, base_url, &state.token))
        .collect();
    let display = store.session_display(username.as_deref().unwrap_or(""));
    drop(store);

    let exports_dir = state.cfg.data_dir.join("exports");
    std::fs::create_dir_all(&exports_dir)
        .map_err(|e| ApiError::internal(format!("create exports dir: {e}")))?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let scope = username.as_deref().unwrap_or("all");
    // `username` is a free-form request parameter — it is only ever matched
    // against feed contents above, never validated against the store — so it
    // cannot go into a file name verbatim. Note the `sns-` prefix does NOT make
    // that safe: Win32 strips trailing dots, so `sns-..` normalizes to a literal
    // `sns-` directory and the rest of the payload keeps traversing. Slugify,
    // then assert containment (see `pathsafe`).
    let file_name = format!(
        "sns-{}-{stamp}.{format}",
        crate::pathsafe::slugify(scope, "scope")
    );
    let path = exports_dir.join(&file_name);
    if !crate::pathsafe::is_contained(&exports_dir, &path) {
        return Err(ApiError::internal("export path escaped the exports dir"));
    }

    let bytes: Vec<u8> = if format == "html" {
        let mut body_html = String::new();
        for e in &entries {
            let media_html: String = e["media"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|m| {
                            let u = m["thumb"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .or_else(|| m["url"].as_str())
                                .unwrap_or("");
                            format!(r#"<div class="media"><a href="{u}">{u}</a></div>"#)
                        })
                        .collect()
                })
                .unwrap_or_default();
            body_html.push_str(&format!(
                "<article><header><b>{}</b> <time>{}</time></header><p>{}</p>{}</article>\n",
                e["displayName"].as_str().unwrap_or(""),
                e["createTime"],
                html_escape(e["content"].as_str().unwrap_or("")),
                media_html,
            ));
        }
        let html = format!(
            "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<title>SNS export — {}</title><style>body{{font-family:system-ui;max-width:720px;margin:2rem auto}}\
article{{border-bottom:1px solid #ddd;padding:.8rem 0}}</style></head>\
<body><h1>朋友圈导出</h1><p>account={} scope={} count={}</p>{}</body></html>",
            html_escape(&display),
            html_escape(&account.info.wxid),
            html_escape(scope),
            entries.len(),
            body_html
        );
        html.into_bytes()
    } else {
        serde_json::to_vec_pretty(&json!({
            "generator": "weflow-server",
            "exportedAt": chrono::Utc::now().timestamp(),
            "account": account.info.wxid,
            "scope": scope,
            "count": entries.len(),
            "stats": stats,
            "timeline": entries,
        }))
        .map_err(|e| ApiError::internal(format!("serialize export: {e}")))?
    };
    std::fs::write(&path, &bytes)
        .map_err(|e| ApiError::internal(format!("write export: {e}")))?;

    Ok(Json(json!({
        "success": true,
        "file": file_name,
        "path": path.display().to_string(),
        "count": entries.len(),
        "bytes": bytes.len(),
        "format": format,
        "stats": stats,
    })))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// GET /api/v1/sns/export/stats — aggregation + latest export artifact info.
pub async fn export_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    let account = ready_account(&state, &params)?;

    let stats = {
        let store = account.store.read();
        aggregate(&store.sns_feeds)
    };
    // report the most recent export artifact if any
    let exports_dir = state.cfg.data_dir.join("exports");
    let mut last_export = None;
    if let Ok(entries) = std::fs::read_dir(&exports_dir) {
        let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        for e in entries.flatten() {
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_file() {
                continue;
            }
            let modified = md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, e.path()));
            }
        }
        if let Some((t, p)) = best {
            last_export = Some(json!({
                "file": p.file_name().map(|f| f.to_string_lossy().to_string()),
                "path": p.display().to_string(),
                "bytes": std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
                "exportedAt": t.duration_since(std::time::UNIX_EPOCH).ok()
                    .and_then(|d| i64::try_from(d.as_secs()).ok()),
            }));
        }
    }
    let my_wxid = account.info.wxid.clone();
    let my_posts = {
        // recompute from the store without holding the lock across await-free code
        let store = account.store.read();
        store
            .sns_feeds
            .iter()
            .filter(|f| f.user_name == my_wxid)
            .count()
    };
    if crate::server::flex_bool(&params, "verbose") {
        return Ok(Json(json!({
            "success": true,
            "stats": stats,
            "lastExport": last_export,
        })));
    }
    Ok(Json(json!({
        "success": true,
        "data": {
            "totalPosts": stats["feeds"],
            "totalFriends": stats["posters"],
            "myPosts": my_posts,
        },
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/sns/media/proxy?url=…&referer=&user_agent=
//
// Server-side relay for moment media CDN references. Strict host allowlist
// (*.qpic.cn / *.qpic.com) to prevent SSRF. Payloads are sniffed: standard
// images pass through, wxgf (raw HEVC still) is converted to PNG via ffmpeg;
// anything the CDN refuses is surfaced as a structured error (the mmsns CDN
// requires WeChat-client request context that a plain GET cannot provide).
// ---------------------------------------------------------------------------

const PROXY_HOST_SUFFIXES: [&str; 3] = ["qpic.cn", "qpic.com", "qq.com"];

/// Redirect hops this endpoint will follow. Each one is re-checked against the
/// allow-list before it is issued, so this only bounds the work, not the trust.
const PROXY_MAX_REDIRECTS: usize = 5;

#[cfg(windows)]
const CURL_BIN: &str = "curl.exe";
#[cfg(not(windows))]
const CURL_BIN: &str = "curl";

pub async fn media_proxy(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<axum::extract::Json<serde_json::Value>>,
) -> ApiResult<Response> {
    use axum::body::Body;
    use axum::http::{header, StatusCode};

    let params = extract_params(&query, body);
    require_auth(&state, &params, &headers)?;
    ready_account(&state, &params)?;

    let url = params
        .get("url")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("url is required"))?;
    if let Err(reason) = check_proxy_url(&url) {
        return Err(ApiError::bad_request(reason));
    }

    let referer = params.get("referer").filter(|s| !s.is_empty()).cloned();
    let user_agent = params.get("user_agent").filter(|s| !s.is_empty()).cloned();

    // Spawning curl and waiting up to 30s per hop is blocking IO; on a tokio
    // worker a handful of concurrent proxy calls would stall unrelated requests.
    let fetched = tokio::task::spawn_blocking(move || {
        proxy_fetch(&url, referer.as_deref(), user_agent.as_deref())
    })
    .await
    .map_err(|e| ApiError::internal(format!("proxy task failed: {e}")))?;

    let (code, bytes) = match fetched {
        Ok(v) => v,
        Err(ProxyFetchError::Spawn(detail)) => {
            return Ok((
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({"error":"proxy_spawn_failed","detail":detail})),
            )
                .into_response())
        }
        // A redirect off the allow-list is the SSRF attempt itself, so it is
        // reported as a rejected request rather than an upstream failure.
        Err(ProxyFetchError::Redirect(detail)) => {
            return Err(ApiError::bad_request(detail));
        }
    };
    if code >= 400 || bytes.is_empty() {
        return Ok((
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({
                "error": "cdn_rejected",
                "upstreamStatus": code,
                "note": "the mmsns CDN requires WeChat-client request context; \
                         raw references cannot be fetched anonymously",
            })),
        )
            .into_response());
    }

    // sniff + wxgf conversion
    let (bytes, ctype) = if bytes.starts_with(b"wxgf") {
        match crate::media::export::wxgf_to_png(&bytes) {
            Some(png) => (png, "image/png"),
            None => (bytes, "video/hevc"),
        }
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        (bytes, "image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        (bytes, "image/png")
    } else if bytes.starts_with(b"GIF8") {
        (bytes, "image/gif")
    } else {
        (bytes, "application/octet-stream")
    };

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (header::CONTENT_LENGTH, bytes.len().to_string()),
        ],
        Body::from(bytes),
    )
        .into_response())
}

enum ProxyFetchError {
    /// curl could not be started at all.
    Spawn(String),
    /// The upstream tried to redirect somewhere the allow-list does not cover.
    Redirect(String),
}

/// Scheme + allow-list gate, applied to the caller's URL *and* to every redirect
/// target before it is followed.
fn check_proxy_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("url must be http(s)".to_string());
    }
    if !PROXY_HOST_SUFFIXES
        .iter()
        .any(|suffix| host_matches(&lower, suffix))
    {
        return Err("url host is not an allowed SNS media host".to_string());
    }
    Ok(())
}

/// One CDN fetch, following redirects by hand.
///
/// Redirects are deliberately NOT delegated to `curl -L`: the allow-list is
/// checked once, in this process, against the URL we are about to request. With
/// `-L`, curl follows 3xx hops itself, so any open redirect under an allowed
/// host — and `qq.com` is a large surface — turns this endpoint into an
/// arbitrary-target fetcher, including loopback and link-local addresses. The
/// gate has to sit on every hop, which means owning the redirect loop.
///
/// Residual, and not fixable here: a hostname under an allowed suffix that
/// resolves to a private address still passes. Closing that needs resolution to
/// happen where the result can be inspected before connecting.
fn proxy_fetch(
    url: &str,
    referer: Option<&str>,
    user_agent: Option<&str>,
) -> Result<(u16, Vec<u8>), ProxyFetchError> {
    let mut current = url.to_string();

    for _ in 0..=PROXY_MAX_REDIRECTS {
        let mut cmd = std::process::Command::new(CURL_BIN);
        // Body to stdout, metadata to stderr: keeping them on separate streams
        // means a binary payload can never be confused for the status line, and
        // avoids a predictable temp file under a world-writable directory.
        cmd.args([
            "-sS",
            "--proto",
            "=http,https",
            "-m",
            "30",
            "-o",
            "-",
            "-w",
            "%{stderr}wfs-meta %{http_code} %{redirect_url}",
        ]);
        if let Some(referer) = referer {
            cmd.arg("-e").arg(referer);
        }
        if let Some(ua) = user_agent {
            cmd.arg("-A").arg(ua);
        }
        cmd.arg(&current);

        let output = cmd
            .output()
            .map_err(|e| ProxyFetchError::Spawn(e.to_string()))?;

        // Diagnostics from -sS share stderr with the metadata, so match the
        // tagged line instead of assuming it is the only thing there.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let meta = stderr
            .lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix("wfs-meta "));
        let (code, redirect) = match meta {
            Some(meta) => {
                let mut parts = meta.split_whitespace();
                let code = parts.next().unwrap_or("").parse::<u16>().unwrap_or(502);
                (code, parts.next().unwrap_or("").trim().to_string())
            }
            // No metadata line means the transfer never completed (DNS, TLS,
            // timeout). Report it as a gateway failure, not a redirect.
            None => return Ok((502, Vec::new())),
        };

        let is_redirect = (300..400).contains(&code) && !redirect.is_empty();
        if !is_redirect {
            return Ok((code, output.stdout));
        }

        check_proxy_url(&redirect).map_err(|reason| {
            ProxyFetchError::Redirect(format!("upstream redirect rejected: {reason}"))
        })?;
        current = redirect;
    }

    Err(ProxyFetchError::Redirect(
        "too many upstream redirects".to_string(),
    ))
}

/// Host of `lower_url`, or None when there is no authority to speak of.
///
/// The authority ends at the first `/`, `?` or `#` — stopping only at `/` reads
/// a query string as part of the host.
///
/// Userinfo precedes the host (`scheme://user:pw@host/`), so it is removed by
/// taking the segment after the LAST `@`. Splitting forward instead returns the
/// *userinfo* as the host, which is the wrong end: `https://qq.com@evil.com/`
/// then "matches" the allow-list while curl connects to `evil.com`.
fn url_host(lower_url: &str) -> Option<&str> {
    let (_, rest) = lower_url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Strip the port. An IPv6 literal keeps its brackets, so it never collides
    // with the allow-list's suffix comparison.
    let host = match host.strip_prefix('[') {
        Some(v6) => return v6.split(']').next().filter(|h| !h.is_empty()),
        None => host.split(':').next().unwrap_or(host),
    };
    (!host.is_empty()).then_some(host)
}

fn host_matches(lower_url: &str, suffix: &str) -> bool {
    let Some(host) = url_host(lower_url) else {
        return false;
    };
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_host_reads_the_authority_not_the_userinfo() {
        assert_eq!(url_host("https://mmsns.qpic.cn/a/b.jpg"), Some("mmsns.qpic.cn"));
        assert_eq!(url_host("https://mmsns.qpic.cn:8443/a"), Some("mmsns.qpic.cn"));
        assert_eq!(url_host("https://qpic.cn"), Some("qpic.cn"));
        // userinfo sits before the host: the host is after the LAST '@'
        assert_eq!(url_host("https://qq.com@evil.example/a"), Some("evil.example"));
        assert_eq!(url_host("https://u:p@evil.example/a"), Some("evil.example"));
        assert_eq!(url_host("https://a@b@evil.example/a"), Some("evil.example"));
        // authority ends at '?' and '#', not only at '/'
        assert_eq!(url_host("https://evil.example?x=.qq.com"), Some("evil.example"));
        assert_eq!(url_host("https://evil.example#.qq.com"), Some("evil.example"));
        // ipv6 keeps its brackets stripped but stays a single host
        assert_eq!(url_host("http://[::1]:8080/x"), Some("::1"));
        // nothing that can be called a host
        assert_eq!(url_host("https:///a"), None);
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn proxy_gate_rejects_the_userinfo_bypass() {
        // legitimate CDN references still pass
        assert!(check_proxy_url("https://mmsns.qpic.cn/mmsns/abc/0").is_ok());
        assert!(check_proxy_url("https://shmmsns.qpic.cn/x.jpg").is_ok());
        assert!(check_proxy_url("https://res.qq.com/x.png").is_ok());
        assert!(check_proxy_url("HTTPS://MMSNS.QPIC.CN/X.JPG").is_ok());

        // the bypass this gate previously accepted: forward-split userinfo made
        // the allow-list read "qq.com" while curl would connect to evil.example
        for probe in [
            "https://qq.com@evil.example/x",
            "https://qpic.cn@169.254.169.254/latest/meta-data/",
            "https://user:pw@qq.com.evil.example/x",
            "https://evil.example?ref=.qq.com",
            "https://evil.example#.qq.com",
            "https://notqq.com/x",
            "https://qq.com.evil.example/x",
            "http://127.0.0.1:9000/admin",
            "file:///c:/windows/win.ini",
            "gopher://qq.com/x",
        ] {
            assert!(check_proxy_url(probe).is_err(), "{probe} must be rejected");
        }

        // suffix matching is on a label boundary, not a substring
        assert!(check_proxy_url("https://xqpic.cn/x").is_err());
        assert!(check_proxy_url("https://sub.mmsns.qpic.cn/x").is_ok());
    }
}
