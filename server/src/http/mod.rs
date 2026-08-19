//! HTTP routing (spec §5). Base path `/v1`, JSON bodies, and the spec §5 error
//! envelope on every failure.

pub mod admin;
pub mod auth;
pub mod context;
pub mod devices;
pub mod extract;
pub mod idempotency;
pub mod ops;
pub mod terminals;
pub mod ws;

use crate::app::AppState;
use crate::error::ApiError;
use axum::Router;
use axum::routing::{delete, get, post};
use std::sync::Arc;

/// Hard ceiling on request bodies, independent of the runtime setting.
///
/// `limits.max_request_body_bytes` is enforced per request from the live settings
/// snapshot; this layer is the schema maximum, so a chunked body cannot consume
/// unbounded memory before that check runs.
const HARD_BODY_LIMIT: usize = 10 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let v1 = Router::new()
        // Registration and authentication
        .route("/v1/auth/challenges", post(auth::create_challenge))
        .route("/v1/identities", post(auth::register_identity))
        .route("/v1/auth/tokens", post(auth::create_token))
        .route(
            "/v1/auth/websocket-tickets",
            post(auth::create_websocket_ticket),
        )
        // Devices
        .route("/v1/devices", post(devices::register).get(devices::list))
        .route("/v1/devices/{device_id}", get(devices::get))
        .route("/v1/devices/{device_id}", delete(devices::revoke))
        // Asking a device to open a terminal (spec §4.6). Deliberately a sub-resource
        // of the *device*, so no route under /v1/terminals gains a mutating method and
        // §5.3's read-only guarantee stays literally true.
        .route(
            "/v1/devices/{device_id}/terminals",
            post(terminals::create_for_device),
        )
        // Terminals
        .route("/v1/terminals", get(terminals::list))
        .route("/v1/terminals/{terminal_id}", get(terminals::get))
        // WebSocket protocols
        .route("/v1/devices/{device_id}/relay", get(ws::relay_upgrade))
        .route(
            "/v1/terminals/{terminal_id}/mirror",
            get(ws::mirror_upgrade),
        )
        // Operator surface
        .route(
            "/v1/admin/settings",
            get(admin::get_settings).patch(admin::patch_settings),
        )
        .route("/v1/admin/settings/audit", get(admin::get_audit))
        .route("/v1/admin/flush", post(admin::flush_now))
        // Operations
        .route("/healthz", get(ops::healthz))
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics_endpoint));

    v1.fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            HARD_BODY_LIMIT,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            context::middleware,
        ))
        .with_state(state)
}

/// The isolated plain-HTTP health listener (spec §4.1 permits exactly this
/// exception). It exposes nothing but liveness and readiness, and does not run the
/// secure-transport middleware.
pub fn health_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(ops::healthz))
        .route("/readyz", get(ops::readyz))
        .fallback(not_found)
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        crate::error::code::INVALID_REQUEST,
        "method not allowed for this path",
    )
}
