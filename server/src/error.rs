//! Error model. Every API failure renders the spec §5 error envelope.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

tokio::task_local! {
    /// Correlation ID for the request currently being served. Set by the
    /// request-context middleware so error rendering can echo it without threading
    /// it through every handler signature.
    pub static REQUEST_ID: String;
}

pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| "-".to_string())
}

/// Stable machine-readable error codes. These are part of the public API contract.
pub mod code {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const VALIDATION_FAILED: &str = "validation_failed";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "unsupported_media_type";
    pub const INTERNAL: &str = "internal_error";
    pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
    pub const SETTINGS_REVISION_CONFLICT: &str = "settings_revision_conflict";
    pub const SETTINGS_INVALID: &str = "settings_invalid";
    pub const CHALLENGE_INVALID: &str = "challenge_invalid";
    pub const CHALLENGE_EXPIRED: &str = "challenge_expired";
    pub const CHALLENGE_CONSUMED: &str = "challenge_consumed";
    pub const SIGNATURE_INVALID: &str = "signature_invalid";
    pub const UNSUPPORTED_ALGORITHM: &str = "unsupported_algorithm";
    pub const DEVICE_REVOKED: &str = "device_revoked";
    pub const LIMIT_EXCEEDED: &str = "limit_exceeded";
    pub const FEATURE_DISABLED: &str = "feature_disabled";
    pub const INSECURE_TRANSPORT: &str = "insecure_transport";
    pub const IDEMPOTENCY_KEY_CONFLICT: &str = "idempotency_key_conflict";
    pub const IDEMPOTENCY_KEY_REQUIRED: &str = "idempotency_key_required";
    pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";
    pub const NOT_READY: &str = "not_ready";
    /// No capable publisher is connected for the device (spec §4.6 condition 4).
    pub const PUBLISHER_UNAVAILABLE: &str = "publisher_unavailable";
    /// The publisher answered, and said no.
    pub const PUBLISHER_DECLINED: &str = "publisher_declined";
    /// The publisher did not answer in time. Deliberately ambiguous: a terminal may
    /// still have been created, so a caller must re-ask rather than guess (spec §5.2).
    pub const PUBLISHER_TIMEOUT: &str = "publisher_timeout";
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
    /// Emitted as `WWW-Authenticate` for 401s.
    pub www_authenticate: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            www_authenticate: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code::INVALID_REQUEST, message)
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            code::VALIDATION_FAILED,
            message,
        )
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        let mut e = Self::new(StatusCode::UNAUTHORIZED, code::UNAUTHORIZED, message);
        e.www_authenticate = Some("Bearer".to_string());
        e
    }

    pub fn insufficient_scope(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code::INSUFFICIENT_SCOPE, message)
    }

    /// Ownership failures deliberately render as 404 (spec §4.4) so that resource
    /// existence is never revealed to a non-owner.
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, code::NOT_FOUND, "Resource not found")
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code::CONFLICT, message)
    }

    pub fn rate_limited(retry_after: u64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            code::RATE_LIMITED,
            "Rate limit exceeded",
        );
        e.retry_after = Some(retry_after);
        e
    }

    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code::LIMIT_EXCEEDED, message)
    }

    pub fn idempotency_key_required(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            code::IDEMPOTENCY_KEY_REQUIRED,
            message,
        )
    }

    /// 503 with a Retry-After: the device may reconnect at any moment, so this is a
    /// "not now", not a "never".
    pub fn publisher_unavailable(message: impl Into<String>) -> Self {
        let mut e = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            code::PUBLISHER_UNAVAILABLE,
            message,
        );
        e.retry_after = Some(5);
        e
    }

    /// 502: the upstream peer answered and refused. Its free-text detail is logged for
    /// the operator and never forwarded to the caller (spec §4.6).
    pub fn publisher_declined(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, code::PUBLISHER_DECLINED, message)
    }

    pub fn publisher_timeout(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            code::PUBLISHER_TIMEOUT,
            message,
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code::INTERNAL, message)
    }

    pub fn storage_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            code::STORAGE_UNAVAILABLE,
            message,
        )
    }

    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = code;
        self
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = current_request_id();
        if self.status.is_server_error() {
            tracing::error!(
                event = "request_error",
                request_id = %request_id,
                code = self.code,
                status = self.status.as_u16(),
                message = %self.message,
                "request failed"
            );
        }
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "request_id": request_id,
            }
        });
        let mut resp = (self.status, axum::Json(body)).into_response();
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        if let Some(challenge) = self.www_authenticate
            && let Ok(v) = HeaderValue::from_str(&challenge)
        {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(format!("database error: {e}"))
    }
}

impl From<r2d2::Error> for ApiError {
    fn from(e: r2d2::Error) -> Self {
        ApiError::storage_unavailable(format!("database pool error: {e}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::invalid(format!("malformed JSON: {e}"))
    }
}
