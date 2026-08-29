//! Endpoint handlers (WeFlow-compatible shapes).

pub mod accounts;
pub mod chatlab_pull;
pub mod contacts;
pub mod group_members;
pub mod health;
pub mod media;
pub mod messages;
pub mod push_events;
pub mod sessions;
pub mod sns;
pub mod sync;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;

use crate::server::error::{ApiError, ApiResult};
use crate::server::{AccountHandle, AppState};

/// Shared extractor: merge GET query params and POST JSON body into one map
/// (body wins). `query` is the handler's `Query(query)` map.
pub fn extract_params<T: serde::de::DeserializeOwned + serde::Serialize>(
    query: &HashMap<String, String>,
    body: Option<Json<T>>,
) -> HashMap<String, String> {
    let mut out = query.clone();
    if let Some(body) = body
        && let Ok(value) = serde_json::to_value(body.0)
            && let Some(map) = value.as_object() {
                for (k, v) in map {
                    out.insert(
                        k.clone(),
                        match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    );
                }
            }
    out
}

// Auth lives in `server::auth`; re-exported so handlers keep importing it
// from `super::` unchanged.
pub use crate::server::auth::{authorized, require_auth};

/// Require a registered, ready account; `wxid` from the `wxid` param, or else
/// the bound account.
///
/// Only one account is ever bound (see `server::bound_account`), so the default
/// is unambiguous. It deliberately does NOT filter on readiness: resolving the
/// binding regardless lets the not-ready branch below answer "indexing or
/// error" instead of the misleading "nothing registered".
pub fn ready_account(
    state: &Arc<AppState>,
    params: &HashMap<String, String>,
) -> ApiResult<Arc<AccountHandle>> {
    let accounts = state.accounts.lock();
    let handle = match params.get("wxid") {
        Some(w) => accounts.get(w).cloned(),
        None => crate::server::bound_account(&accounts),
    };
    let Some(handle) = handle else {
        return Err(ApiError::service_unavailable(
            "no account registered/ready; register via POST /api/v1/accounts",
        ));
    };
    if !handle.status().is_ready() {
        return Err(ApiError::service_unavailable("account not ready (indexing or error)"));
    }
    Ok(handle)
}

/// WeChat `local_type` → canonical ChatLab 0.0.2 `messages[].type`.
///
/// The authority is the published enum (docs.chatlab.fun/standard/chatlab-format),
/// NOT the platform's own numbering: 0 TEXT, 1 IMAGE, 2 VOICE, 3 VIDEO,
/// 4 FILE, 5 EMOJI, 7 LINK, 8 LOCATION, 20-27 interactive (24 SHARE,
/// 25 REPLY, 27 CONTACT), 80 SYSTEM, 81 RECALL, 99 OTHER. **6 is unassigned**
/// — never emit it.
///
/// `parsed` is needed because one WeChat code covers several ChatLab types:
/// 49 (appmsg) is a quote reply, a file, or a link depending on its payload.
/// This is the ChatLab code space only; the native `localType` on
/// `/api/v1/messages` is a separate space that downstream pins, so the two
/// must not be conflated.
pub fn chatlab_type(local_type: i64, parsed: &crate::parser::ParsedMsg) -> i64 {
    match local_type {
        1 => 0,   // TEXT
        3 => 1,   // IMAGE
        34 => 2,  // VOICE
        43 => 3,  // VIDEO
        47 => 5,  // EMOJI
        42 => 27, // CONTACT (名片)
        48 => 8,  // LOCATION (位置)
        50 => 24, // SHARE (视频号)
        // appmsg: most specific first. A refermsg makes the message a quote
        // reply whatever else it carries; otherwise an attachment payload
        // makes it a file; anything else left is a link/card.
        49 => {
            if parsed.reply_to.is_some() {
                25 // REPLY
            } else if crate::parser::appmsg_type(&parsed.raw_content) == Some(6) {
                4 // FILE
            } else {
                7 // LINK
            }
        }
        // Both WeChat codes carry system notices; only the ones that actually
        // decoded a revoke payload are recalls. Splitting on the code alone
        // mislabels a 10002 sysmsg that is not a revoke.
        10000 | 10002 => {
            if parsed.revoke.is_some() {
                81 // RECALL
            } else {
                80 // SYSTEM
            }
        }
        _ => 99, // OTHER
    }
}