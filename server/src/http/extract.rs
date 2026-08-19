//! Extractors that render their rejections in the specification's error envelope.
//!
//! axum's built-in extractors answer with a plain-text body, which would break the
//! guarantee that *every* API failure carries `{"error": {code, message, request_id}}`
//! (spec §5). These thin wrappers delegate to the built-ins and map the rejection
//! onto `ApiError`.

use crate::error::{ApiError, code};
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::{extract::FromRequest, extract::FromRequestParts};

/// JSON request body. Use `axum::Json` for *responses*; this is for requests only.
#[derive(Debug, FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct JsonBody<T>(pub T);

#[derive(Debug, FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct Path<T>(pub T);

#[derive(Debug, FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct Query<T>(pub T);

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        match rejection {
            // An unknown or mistyped field is a validation failure, not a syntax error.
            JsonRejection::JsonDataError(e) => ApiError::validation(clean(&e.body_text())),
            JsonRejection::JsonSyntaxError(e) => ApiError::invalid(clean(&e.body_text())),
            JsonRejection::MissingJsonContentType(_) => ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                code::UNSUPPORTED_MEDIA_TYPE,
                "requests must use Content-Type: application/json",
            ),
            JsonRejection::BytesRejection(_) => ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                code::PAYLOAD_TOO_LARGE,
                "request body could not be read within the configured limit",
            ),
            other => ApiError::invalid(clean(&other.body_text())),
        }
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        // A malformed UUID in the path is a client error, and must not reveal whether
        // any such resource exists.
        ApiError::invalid(clean(&rejection.body_text()))
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::invalid(clean(&rejection.body_text()))
    }
}

/// Strip the multi-line detail serde produces so the message stays a single sentence.
fn clean(text: &str) -> String {
    let single: String = text.replace('\n', " ");
    let trimmed = single.trim();
    if trimmed.len() > 300 {
        format!("{}…", &trimmed[..300])
    } else {
        trimmed.to_string()
    }
}
