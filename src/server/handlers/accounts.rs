//! Account registration, detail and deregistration (client-driven startup,
//! qqflow-server style).
//!
//! - `POST /api/v1/accounts` — register. Body:
//!   `{wxid, key?, keys?: {rel: hex}, img_code?, db_path?}`. The key is
//!   validated deterministically against the account's session.db (page-1
//!   HMAC); on success the live-acquisition index build and a watcher task are
//!   spawned.
//! - `GET /api/v1/accounts` — the account detail `/health` no longer carries.
//! - `DELETE /api/v1/accounts/{wxid}` (alias `POST .../{wxid}/deregister`) —
//!   undo a registration.
//!
//! At most ONE account may be bound at a time (see `server::bound_account`).
//! Registering a second wxid is **rejected** with `account_conflict` and leaves
//! the incumbent untouched; switching accounts takes an explicit
//! deregistration. Re-registering the SAME wxid is **idempotent**: a `ready` /
//! `indexing` account answers `already_ready` / `in_progress` from the live
//! handle — no index rebuild, no watcher abort. Only an `error` (or
//! awaiting-key) account is replaced, so a corrected registration recovers
//! cleanly.
//!
//! Every business rejection is HTTP 200 with the verdict in `state`; only
//! malformed input and auth failures use 4xx.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::{bound_account, AccountStatus, AppState, BindOutcome, DeregisterOutcome};

use super::require_auth;

/// Registration payload. Keys live in memory only and must be re-supplied
/// after a server restart.
#[derive(Clone, Deserialize, Default)]
pub struct AccountBody {
    pub wxid: Option<String>,
    pub key: Option<String>,
    pub keys: Option<std::collections::HashMap<String, String>>,
    pub img_code: Option<String>,
    /// direct image keys (preferred over img_code)
    pub img_aes_key: Option<String>,
    pub img_xor_key: Option<String>,
    pub db_path: Option<String>,
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    // Keep the raw JSON body for proper deserialization (nested `keys` map
    // is lost when flattening to string params).
    let raw_body: Option<serde_json::Value> = body.map(|j| j.0);
    let mut params = query.clone();
    if let Some(v) = &raw_body
        && let Some(map) = v.as_object()
    {
        for (k, val) in map {
            params.insert(
                k.clone(),
                match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
            );
        }
    }
    require_auth(&state, &params, &headers)?;

    let body: AccountBody = match raw_body {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))?,
        None => AccountBody {
            wxid: params.get("wxid").cloned(),
            key: params.get("key").cloned(),
            keys: None,
            img_code: params.get("img_code").cloned(),
            img_aes_key: params.get("img_aes_key").cloned(),
            img_xor_key: params.get("img_xor_key").cloned(),
            db_path: params.get("db_path").cloned(),

        },
    };
    // Cloned out of `body` because `body` moves into `start_account` below,
    // and the conflict verdict has to echo the requested wxid.
    let wxid_in = body
        .wxid
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("wxid required"))?;

    // Fast-path guards, under one lock and BEFORE `start_account` resolves
    // paths or reads session.db. Order matters: `start_account` validates the
    // db_path and the key before it ever reaches the binding, so checking
    // occupancy only in there would answer "key failed page-1 HMAC" to a
    // request that was going to be rejected anyway — telling a caller whether
    // its key is valid for a server it cannot bind. The authoritative check
    // still lives inside `register_account`'s lock; this one just keeps a
    // misconfigured client from making the server stat paths on every retry.
    let current = {
        let accounts = state.accounts.lock();
        match bound_account(&accounts) {
            // A different account holds the binding. `occupied_by` names the
            // incumbent so the client can log which account it is actually
            // talking to instead of retrying forever.
            Some(b) if b.info.wxid != wxid_in => {
                return Ok(Json(json!({
                    "success": true,
                    "wxid": wxid_in,
                    "state": "account_conflict",
                    "occupied_by": b.info.wxid,
                    "occupied_status": b.status(),
                })));
            }
            other => other,
        }
    };
    if let Some(h) = current {
        let idempotent = match h.status() {
            AccountStatus::Ready => Some("already_ready"),
            AccountStatus::Indexing => Some("in_progress"),
            // awaiting_key / error -> accept a (corrected) registration
            _ => None,
        };
        if let Some(state_name) = idempotent {
            return Ok(Json(json!({
                "success": true,
                "wxid": h.info.wxid,
                "state": state_name,
                "status": h.status(),
                "db_storage": h.info.db_storage.to_string_lossy(),
            })));
        }
    }

    // delegate to the shared registration path (spawns the async
    // build/watcher)
    let out = crate::server::start_account(state.clone(), body).await?;

    let (handle, state_name) = match out {
        // The build is spawned by the time we get here.
        BindOutcome::Bound(h) => (h, "accepted"),
        BindOutcome::Existing(h) => (
            h.clone(),
            if h.status().is_ready() { "already_ready" } else { "in_progress" },
        ),
        // Lost the race against a concurrent registration.
        BindOutcome::Occupied { wxid, status } => {
            return Ok(Json(json!({
                "success": true,
                "wxid": wxid_in,
                "state": "account_conflict",
                "occupied_by": wxid,
                "occupied_status": status,
            })));
        }
    };

    Ok(Json(json!({
        "success": true,
        "wxid": handle.info.wxid,
        "state": state_name,
        "status": handle.status(),
        "db_storage": handle.info.db_storage.to_string_lossy(),
    })))
}

/// `GET /api/v1/accounts` — the account detail `/health` no longer carries.
///
/// Token-protected and NOT ready-gated: a client polls this while the account
/// is still `indexing`, which is exactly when the server is not ready. Lists
/// the bound account plus every account the startup scan found but nobody
/// registered (`awaiting_key`), so a client can see what exists here before
/// registering anything.
pub async fn list_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    // GET carries no body, so the token arrives via headers or query string.
    require_auth(&state, &query, &headers)?;
    Ok(Json(json!({ "success": true, "accounts": state.account_views() })))
}

/// `DELETE /api/v1/accounts/{wxid}` (and the `POST .../{wxid}/deregister`
/// alias) — undo a registration and return the server to its unregistered
/// state.
///
/// The `wxid` in the path is a safety interlock, not a selector: there is only
/// ever one binding, so naming the wrong account is a client bug worth
/// reporting (`wxid_mismatch`) rather than silently deregistering whatever
/// happens to be bound.
///
/// Token-protected, NOT ready-gated (an account stuck in `error` — or one still
/// `indexing` — is exactly what a client needs to be able to clear), and every
/// business outcome is HTTP 200 with the verdict in `state`.
///
/// `purge_media` defaults to **false**: exported media is derived data the
/// client may still be serving from its own cache, deleting files is not
/// undoable, and the export layout is keyed by talker with no account
/// dimension, so a purge can remove files another account exported for the
/// same talker.
pub async fn delete_handler(
    State(state): State<Arc<AppState>>,
    UrlPath(wxid): UrlPath<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> ApiResult<Json<serde_json::Value>> {
    let params = crate::server::merge_params(&Query(query), body.map(|j| j.0));
    require_auth(&state, &params, &headers)?;
    if wxid.is_empty() {
        return Err(ApiError::bad_request("wxid required"));
    }
    let purge_media = crate::server::flex_bool(&params, "purge_media");

    // `deregister_account` blocks: it takes the store write lock and removes
    // files when purging, so it must not run on an async runtime poll thread.
    let state_for_task = state.clone();
    let wxid_for_task = wxid.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::server::deregister_account(&state_for_task, &wxid_for_task, purge_media)
    })
    .await
    .map_err(|e| ApiError::internal(format!("注销任务失败: {e}")))?;

    let out = match outcome {
        DeregisterOutcome::Deregistered { previous, index_cleared, purged_dirs } => json!({
            "success": true,
            "wxid": wxid,
            "state": "deregistered",
            // The state the account was in when the request landed — lets a
            // client tell "I cancelled an in-flight build" from "I unbound a
            // ready account".
            "previous_status": previous,
            "index_cleared": index_cleared,
            "purged_media": purge_media,
            "purged_dirs": purged_dirs,
        }),
        // Nothing was bound. Idempotent by design: a client that retries a
        // deregistration it already completed gets a 200, not an error.
        DeregisterOutcome::NotRegistered => json!({
            "success": true,
            "wxid": wxid,
            "state": "not_registered",
            "index_cleared": false,
            "purged_media": false,
            "purged_dirs": 0,
        }),
        // The interlock tripped: a different account holds the binding and is
        // left completely untouched.
        DeregisterOutcome::WxidMismatch { occupied_by, status } => json!({
            "success": true,
            "wxid": wxid,
            "state": "wxid_mismatch",
            "occupied_by": occupied_by,
            "occupied_status": status,
            "index_cleared": false,
            "purged_media": false,
            "purged_dirs": 0,
        }),
    };
    Ok(Json(out))
}
