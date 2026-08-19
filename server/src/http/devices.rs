//! Device registration, listing and revocation (spec §5.2).

use super::auth::Auth;
use super::context::RequestContext;
use super::extract::{Path, Query};
use super::idempotency;
use crate::app::AppState;
use crate::crypto::{DeviceRole, Operation, PublicKey, SigningInput};
use crate::db::in_txn;
use crate::db::repo::{self, Cursor, Device};
use crate::error::{ApiError, ApiResult, code};
use crate::metrics;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use crate::util::{b64_decode, to_rfc3339};
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

fn device_json(state: &Arc<AppState>, snapshot: &Snapshot, device: &Device) -> Value {
    // What a phone needs to decide whether asking this machine for a terminal could
    // work at all, rather than discovering it from a 503 (spec §5.2).
    let open_supported = snapshot.bool(keys::FEATURES_TERMINAL_CREATE_ENABLED)
        && state
            .registry
            .publisher_supports_open_request(device.device_id);
    json!({
        "publisher_connected": state.registry.publisher_connected(device.device_id),
        "terminal_open_supported": open_supported,
        "device_id": device.device_id,
        "identity_id": device.identity_id,
        "name": device.name,
        "role": device.role.as_str(),
        "key": {
            "algorithm": device.algorithm,
            "fingerprint": device.key_fingerprint,
        },
        "created_at": to_rfc3339(device.created_at),
        "last_seen_at": device.last_seen_at.map(to_rfc3339),
        "revoked_at": device.revoked_at.map(to_rfc3339),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterDeviceRequest {
    pub name: String,
    pub key: super::auth::KeyInput,
    pub challenge_id: String,
    /// Signature made by the *proposed device key* over the challenge.
    pub device_signature: String,
    /// `publisher`, `client`, or `both`. Defaults to `publisher`, so a request that
    /// predates roles keeps its original meaning (spec §3.2).
    #[serde(default)]
    pub role: Option<String>,
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    auth.require_identity()?;
    auth.require_scope("devices:write")?;

    let snapshot = Arc::clone(&context.snapshot);
    if !snapshot.bool(keys::FEATURES_DEVICE_REGISTRATION_ENABLED) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            code::FEATURE_DISABLED,
            "device registration is disabled",
        ));
    }

    if let Some(replayed) = idempotency::lookup(
        &state,
        &context,
        &auth.identity_id,
        "POST",
        "/v1/devices",
        &body,
    )
    .await?
    {
        return Ok(replayed);
    }

    let request: RegisterDeviceRequest = serde_json::from_slice(&body)?;

    let name = request.name.trim().to_string();
    if name.is_empty() || name.len() > snapshot.usize(keys::LIMITS_MAX_DEVICE_NAME_BYTES) {
        return Err(ApiError::validation(
            "name must be non-empty and within the configured length",
        ));
    }

    let role = match request.role.as_deref() {
        None => DeviceRole::Publisher,
        Some(raw) => DeviceRole::parse(raw)
            .ok_or_else(|| ApiError::validation("role must be publisher, client, or both"))?,
    };

    let key = PublicKey::parse(
        &request.key.algorithm,
        &request.key.public_key,
        &snapshot.list(keys::AUTH_SUPPORTED_KEY_ALGORITHMS),
    )?;
    let signature = b64_decode(&request.device_signature)
        .ok_or_else(|| ApiError::invalid("device_signature must be base64url-encoded"))?;

    // The challenge is consumed by this attempt whether or not it verifies.
    let challenge_id = request.challenge_id.clone();
    let db = state.db.clone();
    let claim = db
        .call(move |conn| in_txn(conn, |txn| repo::claim_challenge(txn, &challenge_id)))
        .await?;
    metrics::CHALLENGES_CONSUMED.inc();

    let record = match claim {
        repo::ChallengeClaim::Claimed(record) => record,
        repo::ChallengeClaim::AlreadyConsumed => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_CONSUMED,
                "this challenge has already been used",
            ));
        }
        repo::ChallengeClaim::Expired => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_EXPIRED,
                "this challenge has expired",
            ));
        }
        repo::ChallengeClaim::Unknown => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_INVALID,
                "unknown challenge",
            ));
        }
    };

    if record.operation != Operation::RegisterDevice {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::CHALLENGE_INVALID,
            "challenge was not issued for register_device",
        ));
    }

    // The challenge must be bound to the authenticated identity and to exactly this
    // proposed device key (spec §5.2).
    let fingerprint = key.fingerprint();
    if record.owner_identity_id.as_deref() != Some(auth.identity_id.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::CHALLENGE_INVALID,
            "challenge is bound to a different owner identity",
        ));
    }
    if record.device_key_fingerprint.as_deref() != Some(fingerprint.as_str())
        || record.key_fingerprint != fingerprint
    {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::CHALLENGE_INVALID,
            "challenge is bound to a different device key",
        ));
    }

    let message = SigningInput {
        origin: &snapshot.string(keys::SERVER_PUBLIC_ORIGIN),
        challenge_id: &record.challenge_id,
        challenge: &record.challenge,
        operation: record.operation,
        key_fingerprint: &record.key_fingerprint,
        owner_identity_id: record.owner_identity_id.as_deref().unwrap_or(""),
        device_key_fingerprint: record.device_key_fingerprint.as_deref().unwrap_or(""),
        expires_at_unix_ms: record.expires_at.timestamp_millis().max(0) as u64,
    }
    .encode();

    // Proof of possession of the device private key, in addition to the identity
    // token that authorises the registration.
    if !key.verify(&message, &signature) {
        metrics::AUTH_FAILURES.inc();
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::SIGNATURE_INVALID,
            "device_signature does not verify against the challenge",
        ));
    }

    let max_devices = snapshot.int(keys::LIMITS_MAX_DEVICES_PER_IDENTITY);
    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    let device = db
        .call(move |conn| {
            if let Some(existing) = repo::get_device_by_fingerprint(conn, &fingerprint)? {
                return Err(ApiError::conflict(format!(
                    "this device key is already registered as device {}",
                    existing.device_id
                )));
            }
            let count = repo::count_active_devices(conn, &identity_id)?;
            if count >= max_devices {
                return Err(ApiError::limit_exceeded(format!(
                    "identity already owns {count} devices, the limit is {max_devices}"
                )));
            }
            in_txn(conn, |txn| {
                repo::insert_device(txn, &identity_id, &key, &name, role)
            })
        })
        .await?;

    tracing::info!(
        event = "device_registered",
        request_id = %context.request_id,
        identity_id = %auth.identity_id,
        device_id = %device.device_id,
        role = device.role.as_str(),
        "device registered"
    );

    let response = device_json(&state, &snapshot, &device);
    idempotency::store(
        &state,
        &context,
        &auth.identity_id,
        "POST",
        "/v1/devices",
        &body,
        StatusCode::CREATED,
        &response,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    // Deliberately not identity-only, and the only method on this resource that is not.
    // Spec §4.4 lets a `client` or `both` device list the devices of the identity that
    // owns it, because that is how a paired phone learns which machine it could ask for
    // a terminal (spec §4.6) — a request that must name a device. The query below is
    // scoped to `auth.identity_id`, which a device token carries just as an identity
    // token does, so this reveals nothing the device could not already infer from its
    // terminal list. `get`, `register` and `revoke` stay identity-only, and
    // `devices:read` is not in a client device's default scopes.
    auth.require_scope("devices:read")?;

    let snapshot = &context.snapshot;
    let limit = resolve_limit(query.limit, snapshot)?;
    let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;

    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    // One extra row tells us whether another page exists.
    let mut devices = db
        .call(move |conn| repo::list_devices(conn, &identity_id, cursor.as_ref(), limit + 1))
        .await?;

    let next_cursor = if devices.len() as i64 > limit {
        devices.truncate(limit as usize);
        devices
            .last()
            .map(|d| Cursor::encode(&to_rfc3339(d.created_at), &d.device_id.to_string()))
    } else {
        None
    };

    Ok(Json(json!({
        "devices": devices
            .iter()
            .map(|device| device_json(&state, snapshot, device))
            .collect::<Vec<_>>(),
        "next_cursor": next_cursor,
    }))
    .into_response())
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    Path(device_id): Path<Uuid>,
) -> ApiResult<Response> {
    auth.require_identity()?;
    auth.require_scope("devices:read")?;

    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    let device = db
        .call(move |conn| repo::get_owned_device(conn, &identity_id, device_id))
        .await?
        // Not owned and not existing are indistinguishable (spec §4.4).
        .ok_or_else(ApiError::not_found)?;

    Ok(Json(device_json(&state, &context.snapshot, &device)).into_response())
}

pub async fn revoke(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    Path(device_id): Path<Uuid>,
) -> ApiResult<Response> {
    auth.require_identity()?;
    auth.require_scope("devices:write")?;

    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    let outcome = db
        .call(move |conn| {
            let device = repo::get_owned_device(conn, &identity_id, device_id)?;
            let Some(device) = device else {
                return Ok(None);
            };
            // Revocation is a security-critical mutation: it commits before the API
            // reports success (spec §7.2).
            let changed = in_txn(conn, |txn| repo::revoke_device(txn, device_id))?;
            Ok(Some((device, changed)))
        })
        .await?;

    let Some((device, changed)) = outcome else {
        return Err(ApiError::not_found());
    };

    if changed {
        metrics::DEVICE_REVOCATIONS.inc();
        tracing::info!(
            event = "device_revoked",
            request_id = %context.request_id,
            identity_id = %auth.identity_id,
            device_id = %device_id,
            "device revoked; terminating its access"
        );
    }

    // Existing tokens and WebSockets must stop working promptly, and no later than
    // thirty seconds after revocation (spec §5.2).
    let registry = Arc::clone(&state.registry);
    tokio::spawn(async move {
        registry.enforce_device_revocation(device_id).await;
    });

    // Idempotent: revoking twice still reports success.
    Ok(Json(json!({
        "device_id": device.device_id,
        "revoked_at": to_rfc3339(device.revoked_at.unwrap_or_else(crate::util::now)),
    }))
    .into_response())
}

pub fn resolve_limit(
    requested: Option<i64>,
    snapshot: &crate::settings::Snapshot,
) -> ApiResult<i64> {
    let max = snapshot.int(keys::LIMITS_MAX_PAGE_SIZE);
    match requested {
        None => Ok(snapshot.int(keys::LIMITS_DEFAULT_PAGE_SIZE).min(max)),
        Some(limit) if limit >= 1 && limit <= max => Ok(limit),
        Some(_) => Err(ApiError::validation(format!(
            "limit must be between 1 and {max}"
        ))),
    }
}
