//! Health, readiness and metrics endpoints (spec §5.4).

use super::context::RequestContext;
use crate::app::AppState;
use crate::error::{ApiError, ApiResult, code};
use crate::metrics;
use crate::settings::defs::keys;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

/// Liveness. Deliberately depends on nothing external: it answers as long as the
/// process is running (spec §5.4).
pub async fn healthz() -> Response {
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

/// Readiness: authentication, durable state, relay acceptance and a loadable
/// settings revision must all be working.
pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    match state.readiness().await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "status": "ready",
                "settings_revision": state.snapshot().revision,
            })),
        )
            .into_response(),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "code": code::NOT_READY,
                    "message": reason,
                    "request_id": crate::error::current_request_id(),
                }
            })),
        )
            .into_response(),
    }
}

/// Metrics. Requires operator authentication unless an operator has explicitly
/// disabled that, and never exposes terminal output, keys, tokens or user labels
/// (spec §5.4, §9).
pub async fn metrics_endpoint(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    if !snapshot.bool(keys::FEATURES_METRICS_ENDPOINT_ENABLED) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            code::FEATURE_DISABLED,
            "the metrics endpoint is disabled",
        ));
    }

    if snapshot.bool(keys::METRICS_REQUIRE_OPERATOR_AUTH) {
        let expected = snapshot
            .secret(keys::AUTH_OPERATOR_TOKEN_HASH)
            .ok_or_else(|| ApiError::internal("no operator credential is configured"))?;
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(|v| crate::crypto::hash_operator_token(v.trim()))
            .ok_or_else(|| ApiError::unauthorized("operator authentication required"))?;
        if !crate::util::ct_eq(presented.as_bytes(), expected.as_bytes()) {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::unauthorized("operator authentication failed"));
        }
    }

    metrics::STORAGE_BYTES.set(state.db.storage_bytes() as i64);
    let body = metrics::render();

    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}
