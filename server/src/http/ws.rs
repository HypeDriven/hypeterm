//! WebSocket upgrade handling for both protocols (spec §6).
//!
//! Authentication, authorisation, scope and limit checks all happen *before* the
//! upgrade, so an unauthorised client never reaches a WebSocket at all.

use super::auth::authenticate_upgrade;
use super::context::RequestContext;
use super::extract::Path;
use crate::app::AppState;
use crate::crypto::{PrincipalKind, scope};
use crate::db::repo;
use crate::error::{ApiError, ApiResult, code};
use crate::metrics;
use crate::relay::messages::{
    ProtocolVersion, SUBPROTOCOL_MIRROR_V1, SUBPROTOCOL_MIRROR_V2, SUBPROTOCOL_PUBLISHER_V1,
    SUBPROTOCOL_PUBLISHER_V2,
};
use crate::relay::mirror::{self, MirrorContext};
use crate::relay::publisher::{self, PublisherContext};
use crate::settings::defs::keys;
use crate::util::new_ulid;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use std::sync::Arc;
use uuid::Uuid;

/// Negotiate the protocol version, preferring version 2 when the client offers it.
///
/// A client must name a subprotocol explicitly (spec §6); offering neither is an
/// error rather than a silent downgrade.
fn negotiate(
    headers: &HeaderMap,
    v1: &'static str,
    v2: &'static str,
) -> ApiResult<ProtocolVersion> {
    let offered = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    ProtocolVersion::negotiate(offered, v1, v2).ok_or_else(|| {
        ApiError::invalid(format!(
            "this endpoint requires the {v1} or {v2} WebSocket subprotocol"
        ))
    })
}

fn check_connection_limits(
    state: &Arc<AppState>,
    context: &RequestContext,
    principal: &str,
) -> ApiResult<crate::relay::registry::ConnectionPermit> {
    let snapshot = &context.snapshot;

    if snapshot.bool(keys::RATELIMIT_ENABLED)
        && !state.rate_limiter.check(
            "ws_connect",
            principal,
            snapshot.int(keys::RATELIMIT_WEBSOCKET_CONNECTIONS_PER_MINUTE_PER_PRINCIPAL),
        )
    {
        metrics::RATE_LIMITED_REQUESTS.inc();
        return Err(ApiError::rate_limited(
            snapshot.u64(keys::RATELIMIT_RETRY_AFTER_SECONDS),
        ));
    }

    state
        .registry
        .acquire_connection(
            principal,
            snapshot.usize(keys::LIMITS_MAX_CONNECTIONS_PER_PRINCIPAL),
        )
        .ok_or_else(|| {
            ApiError::limit_exceeded(
                "this principal already has the maximum number of open connections",
            )
        })
}

pub async fn relay_upgrade(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    Path(device_id): Path<Uuid>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    if !snapshot.bool(keys::FEATURES_PUBLISH_ENABLED) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            code::FEATURE_DISABLED,
            "publishing is disabled",
        ));
    }
    if state.is_shutting_down() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            code::NOT_READY,
            "server is shutting down and is not accepting new connections",
        ));
    }
    let version = negotiate(&headers, SUBPROTOCOL_PUBLISHER_V1, SUBPROTOCOL_PUBLISHER_V2)?;

    let path = format!("/v1/devices/{device_id}/relay");
    let auth = authenticate_upgrade(&state, &snapshot, &headers, &path).await?;

    // A device may only publish as itself (spec §4.4).
    if auth.kind != PrincipalKind::Device || auth.principal_id != device_id.to_string() {
        return Err(ApiError::not_found());
    }
    auth.require_scope(scope::TERMINALS_PUBLISH)?;

    let db = state.db.clone();
    let device = db
        .call(move |conn| repo::get_device(conn, device_id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    if device.revoked_at.is_some() {
        return Err(
            ApiError::unauthorized("this device has been revoked").with_code(code::DEVICE_REVOKED)
        );
    }
    // A client-role device holds no publishing authority (spec §3.2).
    if !device.role.may_publish() {
        return Err(ApiError::insufficient_scope(
            "this device's role does not permit publishing",
        ));
    }

    let permit = check_connection_limits(&state, &context, &auth.principal_id)?;

    let connection_id = new_ulid();
    // Only a version 2 publisher can receive input, so only it gets a channel.
    let (input_tx, input_rx) = if version.supports_input() {
        let (tx, rx) =
            tokio::sync::mpsc::channel(snapshot.usize(keys::LIMITS_MAX_INPUT_QUEUE_FRAMES).max(1));
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let lease = state
        .registry
        .attach_publisher(device_id, &connection_id, version, input_tx);

    let db = state.db.clone();
    let _ = db
        .call(move |conn| repo::touch_device(conn, device_id))
        .await;

    let publisher_context = PublisherContext {
        registry: Arc::clone(&state.registry),
        device,
        connection_id,
        shutdown: state.shutdown_rx.clone(),
        version,
        input_rx,
    };

    Ok(upgrade
        .protocols([version.publisher_subprotocol()])
        .on_upgrade(move |socket| async move {
            publisher::handle(socket, publisher_context, lease, permit).await;
        }))
}

pub async fn mirror_upgrade(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    Path(terminal_id): Path<Uuid>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    if !snapshot.bool(keys::FEATURES_MIRROR_ENABLED) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            code::FEATURE_DISABLED,
            "mirroring is disabled",
        ));
    }
    if state.is_shutting_down() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            code::NOT_READY,
            "server is shutting down and is not accepting new connections",
        ));
    }
    let version = negotiate(&headers, SUBPROTOCOL_MIRROR_V1, SUBPROTOCOL_MIRROR_V2)?;

    let path = format!("/v1/terminals/{terminal_id}/mirror");
    let auth = authenticate_upgrade(&state, &snapshot, &headers, &path).await?;

    // An identity may mirror its own terminals; so may a client-role device it owns,
    // which is what lets a phone hold its own credential rather than the root key
    // (spec §3.2, §4.4).
    if auth.kind == PrincipalKind::Device {
        let device_id = Uuid::parse_str(&auth.principal_id).unwrap_or_default();
        let db = state.db.clone();
        let device = db
            .call(move |conn| repo::get_device(conn, device_id))
            .await?
            .ok_or_else(ApiError::not_found)?;
        if !device.role.may_mirror() || device.identity_id != auth.identity_id {
            return Err(ApiError::not_found());
        }
    }
    auth.require_scope(scope::TERMINALS_MIRROR)?;

    // Ownership is confirmed before the upgrade; a non-owner sees a plain 404.
    let db = state.db.clone();
    let row = db
        .call(move |conn| repo::get_terminal(conn, terminal_id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    if row.identity_id != auth.identity_id {
        return Err(ApiError::not_found());
    }

    let permit = check_connection_limits(&state, &context, &auth.principal_id)?;

    let mirror_context = MirrorContext {
        registry: Arc::clone(&state.registry),
        terminal_id,
        identity_id: auth.identity_id.clone(),
        connection_id: new_ulid(),
        shutdown: state.shutdown_rx.clone(),
        version,
        // Necessary but not sufficient: the remaining §4.5 conditions are re-checked
        // for every frame, because they can change while the subscription is open.
        may_send_input: version.supports_input()
            && auth.require_scope(scope::TERMINALS_INPUT).is_ok(),
        principal_id: auth.principal_id.clone(),
    };

    Ok(upgrade
        .protocols([version.mirror_subprotocol()])
        .on_upgrade(move |socket| async move {
            mirror::handle(socket, mirror_context, permit).await;
        }))
}
