//! `Idempotency-Key` support for mutating requests (spec §10).
//!
//! Records are retained for at least 24 hours, and replaying a key with a different
//! body is a conflict rather than a silent overwrite.

use super::context::RequestContext;
use crate::app::AppState;
use crate::db::repo;
use crate::error::{ApiError, ApiResult, code};
use crate::util::sha256_hex;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub const HEADER: &str = "idempotency-key";

/// Read the header once, during request-context construction.
pub fn extract(headers: &axum::http::HeaderMap, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    headers
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() <= 255)
}

pub(crate) fn key_hash(scope: &str, method: &str, path: &str, key: &str) -> String {
    sha256_hex(format!("{scope}\u{1}{method}\u{1}{path}\u{1}{key}").as_bytes())
}

/// Return a previously stored response for this key, if any.
pub async fn lookup(
    state: &Arc<AppState>,
    context: &RequestContext,
    scope: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> ApiResult<Option<Response>> {
    let Some(key) = context.idempotency_key.clone() else {
        return Ok(None);
    };

    let hash = key_hash(scope, method, path, &key);
    let scope_owned = scope.to_string();
    let db = state.db.clone();
    let existing = db
        .call(move |conn| repo::get_idempotent(conn, &hash, &scope_owned))
        .await?;

    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing.request_hash != sha256_hex(body) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            code::IDEMPOTENCY_KEY_CONFLICT,
            "this Idempotency-Key was already used with a different request body",
        ));
    }

    let status = StatusCode::from_u16(existing.status).unwrap_or(StatusCode::OK);
    let value: serde_json::Value =
        serde_json::from_str(&existing.body).unwrap_or(serde_json::Value::Null);
    Ok(Some((status, axum::Json(value)).into_response()))
}

#[allow(clippy::too_many_arguments)]
pub async fn store(
    state: &Arc<AppState>,
    context: &RequestContext,
    scope: &str,
    method: &str,
    path: &str,
    body: &[u8],
    status: StatusCode,
    response: &serde_json::Value,
) -> ApiResult<()> {
    let Some(key) = context.idempotency_key.clone() else {
        return Ok(());
    };

    let hash = key_hash(scope, method, path, &key);
    let request_hash = sha256_hex(body);
    let scope_owned = scope.to_string();
    let method = method.to_string();
    let path = path.to_string();
    let response = response.to_string();
    let db = state.db.clone();
    db.call(move |conn| {
        repo::put_idempotent(
            conn,
            &hash,
            &scope_owned,
            &method,
            &path,
            &request_hash,
            status.as_u16(),
            &response,
        )
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_respects_the_feature_switch_and_bounds() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(HEADER, "  abc  ".parse().unwrap());
        assert_eq!(extract(&headers, true).as_deref(), Some("abc"));
        assert_eq!(extract(&headers, false), None);

        let mut long = axum::http::HeaderMap::new();
        long.insert(HEADER, "x".repeat(300).parse().unwrap());
        assert_eq!(extract(&long, true), None);
    }

    #[test]
    fn key_hashes_separate_scope_method_and_path() {
        let a = key_hash("identity-a", "POST", "/v1/devices", "k");
        let b = key_hash("identity-b", "POST", "/v1/devices", "k");
        let c = key_hash("identity-a", "POST", "/v1/other", "k");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
