//! Unified API error envelope: `{"success":false,"code":<http>,"message":...}`.
//!
//! `reason` 只在**客户端可据以自动处置**的冲突上出现，是机器字段：客户端按它
//! 决定「重新从头拉」之类的动作，不要去匹配中文 `message`。当前只有游标失效
//! （`cursor_reset_required`）用它。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub success: bool,
    pub code: u16,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// 机器可读的失效原因，见 [`crate::server::cursor::CursorError`]。
    pub reason: Option<&'static str>,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into(), reason: None }
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.into(), reason: None }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.into(), reason: None }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, message: msg.into(), reason: None }
    }
    /// 冲突 + 机器可读原因：客户端据此决定重拉，而不是重试。
    pub fn conflict_with_reason(reason: &'static str, msg: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, message: msg.into(), reason: Some(reason) }
    }
    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, message: msg.into(), reason: None }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into(), reason: None }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.status.as_u16();
        let body = ApiErrorBody { success: false, code, message: self.message, reason: self.reason };
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
