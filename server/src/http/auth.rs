//! Proof-of-possession registration and authentication (spec §4.2, §4.3, §5.1).

use super::context::{PrincipalLog, RequestContext};
use super::extract::JsonBody;
use super::idempotency;
use crate::app::{AppState, build_claims};
use crate::crypto::{
    CHALLENGE_SIGNATURE_CONTEXT, DeviceRole, Operation, PrincipalKind, PublicKey, SigningInput,
    hash_ticket, new_ticket, scope,
};
use crate::db::in_txn;
use crate::db::repo::{self, ChallengeClaim};
use crate::error::{ApiError, ApiResult, code};
use crate::metrics;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use crate::util::{b64_decode, b64_encode, new_ulid, now, random_bytes, to_rfc3339};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, State};
use axum::http::{StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

// ------------------------------------------------------------------- extractors

/// An authenticated identity or device.
#[derive(Debug, Clone)]
pub struct Auth {
    pub kind: PrincipalKind,
    pub principal_id: String,
    pub identity_id: String,
    pub scopes: Vec<String>,
    pub jti: String,
}

impl Auth {
    pub fn require_scope(&self, scope: &str) -> ApiResult<()> {
        if self.scopes.iter().any(|s| s == scope) {
            Ok(())
        } else {
            Err(ApiError::insufficient_scope(format!(
                "this operation requires the {scope} scope"
            )))
        }
    }

    pub fn require_identity(&self) -> ApiResult<()> {
        match self.kind {
            PrincipalKind::Identity => Ok(()),
            PrincipalKind::Device => Err(ApiError::insufficient_scope(
                "this operation requires an identity token",
            )),
        }
    }
}

/// Extract the bearer token, rejecting any attempt to pass it in the query string.
fn bearer_token(parts: &Parts) -> ApiResult<String> {
    if let Some(query) = parts.uri.query() {
        // Token material must never appear in a URL (spec §4.3).
        for forbidden in ["access_token", "token", "ticket", "bearer"] {
            if query.split('&').any(|pair| {
                pair.split('=')
                    .next()
                    .map(|k| k.eq_ignore_ascii_case(forbidden))
                    .unwrap_or(false)
            }) {
                return Err(ApiError::invalid(
                    "credentials must not be supplied in the query string; use the Authorization header",
                ));
            }
        }
    }

    let header = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;

    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| ApiError::unauthorized("Authorization header must use the Bearer scheme"))?;

    Ok(token.trim().to_string())
}

impl FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let context = RequestContext::from_request_parts(parts, state).await?;
        let token = match bearer_token(parts) {
            Ok(token) => token,
            Err(e) => {
                metrics::AUTH_FAILURES.inc();
                return Err(e);
            }
        };

        let claims = match state.tokens.verify(&token, &context.snapshot) {
            Ok(claims) => claims,
            Err(e) => {
                metrics::AUTH_FAILURES.inc();
                return Err(e);
            }
        };

        let auth = Auth {
            kind: claims.principal,
            principal_id: claims.sub.clone(),
            identity_id: claims.identity_id.clone(),
            scopes: claims.scopes.clone(),
            jti: claims.jti.clone(),
        };

        // Revocation is checked against durable state on every request, so a revoked
        // device loses access immediately rather than when its token expires.
        let principal_id = claims.sub.clone();
        let issued_at = claims.iat;
        let kind = claims.principal;
        let db = state.db.clone();
        let revoked = db
            .call(move |conn| {
                if let Some(cutoff) = repo::token_cutoff(conn, &principal_id)?
                    && cutoff.timestamp() >= issued_at
                {
                    return Ok(true);
                }
                if kind == PrincipalKind::Device {
                    let device_id = Uuid::parse_str(&principal_id).unwrap_or_default();
                    match repo::get_device(conn, device_id)? {
                        Some(device) => return Ok(device.revoked_at.is_some()),
                        None => return Ok(true),
                    }
                }
                Ok(false)
            })
            .await?;

        if revoked {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::unauthorized("credential has been revoked")
                .with_code(code::DEVICE_REVOKED));
        }

        // Per-principal request rate limiting.
        if context.snapshot.bool(keys::RATELIMIT_ENABLED)
            && !state.rate_limiter.check(
                "principal",
                &auth.principal_id,
                context
                    .snapshot
                    .int(keys::RATELIMIT_REQUESTS_PER_MINUTE_PER_PRINCIPAL),
            )
        {
            metrics::RATE_LIMITED_REQUESTS.inc();
            return Err(ApiError::rate_limited(
                context.snapshot.u64(keys::RATELIMIT_RETRY_AFTER_SECONDS),
            ));
        }

        if let Some(log) = parts.extensions.get::<PrincipalLog>() {
            log.set(&auth.principal_id);
        }
        metrics::AUTH_SUCCESSES.inc();
        Ok(auth)
    }
}

/// Operator authentication for `/v1/admin` and `/metrics`.
///
/// Deliberately a separate mechanism from identity and device authentication
/// (spec §5.5).
pub struct OperatorAuth {
    pub principal: String,
}

impl FromRequestParts<Arc<AppState>> for OperatorAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let context = RequestContext::from_request_parts(parts, state).await?;
        let token = bearer_token(parts).map_err(|_| {
            metrics::AUTH_FAILURES.inc();
            ApiError::unauthorized("operator authentication required")
        })?;

        let expected = context
            .snapshot
            .secret(keys::AUTH_OPERATOR_TOKEN_HASH)
            .ok_or_else(|| ApiError::internal("no operator credential is configured"))?;
        let presented = crate::crypto::hash_operator_token(&token);

        if !crate::util::ct_eq(presented.as_bytes(), expected.as_bytes()) {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::unauthorized("operator authentication failed"));
        }

        // Operator identity is the credential's own hash prefix: enough to correlate
        // audit entries without recording the credential.
        let principal = format!("operator:{}", &presented[..12.min(presented.len())]);
        if let Some(log) = parts.extensions.get::<PrincipalLog>() {
            log.set(&principal);
        }
        metrics::AUTH_SUCCESSES.inc();
        Ok(OperatorAuth { principal })
    }
}

// -------------------------------------------------------------------- challenges

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyInput {
    pub algorithm: String,
    pub public_key: String,
}

/// Unknown fields are rejected rather than ignored: on the authentication path a
/// silently dropped field could change what the caller believes it authorised
/// (spec §5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    pub operation: String,
    pub key: KeyInput,
    /// Required for `register_device`: the identity that will own the device.
    #[serde(default)]
    pub owner_identity_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    pub challenge_id: String,
    pub challenge: String,
    pub signature_context: &'static str,
    /// The exact bytes to sign, base64url encoded. The server always recomputes and
    /// verifies against its own derivation; this is a convenience for clients.
    pub signing_input: String,
    pub expires_at: String,
    pub key_fingerprint: String,
}

pub async fn create_challenge(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    JsonBody(request): JsonBody<ChallengeRequest>,
) -> ApiResult<Response> {
    let snapshot = &context.snapshot;
    let operation = Operation::parse(&request.operation)
        .ok_or_else(|| ApiError::validation(format!("unknown operation: {}", request.operation)))?;

    let key = PublicKey::parse(
        &request.key.algorithm,
        &request.key.public_key,
        &snapshot.list(keys::AUTH_SUPPORTED_KEY_ALGORITHMS),
    )?;
    let fingerprint = key.fingerprint();

    // Challenges are rate limited by source and by key fingerprint (spec §4.2).
    if snapshot.bool(keys::RATELIMIT_ENABLED) {
        let retry_after = snapshot.u64(keys::RATELIMIT_RETRY_AFTER_SECONDS);
        if !state.rate_limiter.check(
            "challenge_source",
            &context.client_key(),
            snapshot.int(keys::RATELIMIT_CHALLENGES_PER_MINUTE_PER_SOURCE),
        ) {
            metrics::RATE_LIMITED_REQUESTS.inc();
            return Err(ApiError::rate_limited(retry_after));
        }
        if !state.rate_limiter.check(
            "challenge_fingerprint",
            &fingerprint,
            snapshot.int(keys::RATELIMIT_CHALLENGES_PER_MINUTE_PER_FINGERPRINT),
        ) {
            metrics::RATE_LIMITED_REQUESTS.inc();
            return Err(ApiError::rate_limited(retry_after));
        }
    }

    match operation {
        Operation::RegisterIdentity
            if !snapshot.bool(keys::FEATURES_IDENTITY_REGISTRATION_ENABLED) =>
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                code::FEATURE_DISABLED,
                "identity registration is disabled",
            ));
        }
        Operation::RegisterDevice if !snapshot.bool(keys::FEATURES_DEVICE_REGISTRATION_ENABLED) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                code::FEATURE_DISABLED,
                "device registration is disabled",
            ));
        }
        _ => {}
    }

    // A device-registration challenge binds the intended owner and the proposed
    // device key, so it cannot be replayed against a different owner (spec §5.1).
    let owner_identity_id = match operation {
        Operation::RegisterDevice => {
            let owner = request.owner_identity_id.clone().ok_or_else(|| {
                ApiError::validation("owner_identity_id is required for register_device")
            })?;
            let owner_check = owner.clone();
            let db = state.db.clone();
            if !db
                .call(move |conn| repo::identity_exists(conn, &owner_check))
                .await?
            {
                // Do not confirm which identities exist to an unauthenticated caller.
                return Err(ApiError::validation(
                    "owner_identity_id is not a registered identity",
                ));
            }
            Some(owner)
        }
        _ => None,
    };

    let challenge_bytes = random_bytes(snapshot.usize(keys::AUTH_CHALLENGE_BYTES));
    let challenge_id = new_ulid();
    let expires_at =
        now() + chrono::Duration::seconds(snapshot.int(keys::AUTH_CHALLENGE_TTL_SECONDS));

    let device_fingerprint = match operation {
        Operation::RegisterDevice => Some(fingerprint.clone()),
        _ => None,
    };

    let signing_input = SigningInput {
        origin: &snapshot.string(keys::SERVER_PUBLIC_ORIGIN),
        challenge_id: &challenge_id,
        challenge: &challenge_bytes,
        operation,
        key_fingerprint: &fingerprint,
        owner_identity_id: owner_identity_id.as_deref().unwrap_or(""),
        device_key_fingerprint: device_fingerprint.as_deref().unwrap_or(""),
        expires_at_unix_ms: expires_at.timestamp_millis().max(0) as u64,
    }
    .encode();

    let db = state.db.clone();
    let stored_id = challenge_id.clone();
    let stored_challenge = challenge_bytes.clone();
    let stored_owner = owner_identity_id.clone();
    let stored_device_fingerprint = device_fingerprint.clone();
    let stored_key = key.clone();
    // Consuming and creating single-use credentials commits immediately (spec §7.2).
    db.call(move |conn| {
        repo::insert_challenge(
            conn,
            &stored_id,
            operation,
            &stored_key,
            stored_owner.as_deref(),
            stored_device_fingerprint.as_deref(),
            &stored_challenge,
            expires_at,
        )
    })
    .await?;

    metrics::CHALLENGES_ISSUED.inc();
    tracing::info!(
        event = "challenge_issued",
        request_id = %context.request_id,
        operation = operation.as_str(),
        challenge_id = %challenge_id,
        "issued a proof-of-possession challenge"
    );

    Ok((
        StatusCode::CREATED,
        Json(ChallengeResponse {
            challenge_id,
            challenge: b64_encode(&challenge_bytes),
            signature_context: CHALLENGE_SIGNATURE_CONTEXT,
            signing_input: b64_encode(&signing_input),
            expires_at: to_rfc3339(expires_at),
            key_fingerprint: fingerprint,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub challenge_id: String,
    pub signature: String,
}

/// Claim a challenge and verify its signature.
///
/// The challenge is consumed by the attempt itself, so a failed verification cannot
/// be retried against the same challenge (spec §4.2).
async fn verify_challenge(
    state: &Arc<AppState>,
    snapshot: &Snapshot,
    challenge_id: &str,
    signature_b64: &str,
    expected: &[Operation],
) -> ApiResult<repo::ChallengeRecord> {
    let signature = b64_decode(signature_b64)
        .ok_or_else(|| ApiError::invalid("signature must be base64url-encoded"))?;

    let challenge_id = challenge_id.to_string();
    let db = state.db.clone();
    let claim = db
        .call(move |conn| in_txn(conn, |txn| repo::claim_challenge(txn, &challenge_id)))
        .await?;
    metrics::CHALLENGES_CONSUMED.inc();

    let record = match claim {
        ChallengeClaim::Claimed(record) => record,
        ChallengeClaim::AlreadyConsumed => {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_CONSUMED,
                "this challenge has already been used",
            ));
        }
        ChallengeClaim::Expired => {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_EXPIRED,
                "this challenge has expired",
            ));
        }
        ChallengeClaim::Unknown => {
            metrics::AUTH_FAILURES.inc();
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_INVALID,
                "unknown challenge",
            ));
        }
    };

    if !expected.contains(&record.operation) {
        metrics::AUTH_FAILURES.inc();
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::CHALLENGE_INVALID,
            format!("challenge was issued for {}", record.operation.as_str()),
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

    if !record.key.verify(&message, &signature) {
        metrics::AUTH_FAILURES.inc();
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            code::SIGNATURE_INVALID,
            "signature does not verify against the challenge",
        ));
    }

    Ok(record)
}

// -------------------------------------------------------------------- identities

pub async fn register_identity(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    body: Bytes,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    if !snapshot.bool(keys::FEATURES_IDENTITY_REGISTRATION_ENABLED) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            code::FEATURE_DISABLED,
            "identity registration is disabled",
        ));
    }

    let request: SignedRequest = serde_json::from_slice(&body)?;

    // Identity registration is anonymous, so idempotency records are keyed by source.
    let idempotency_scope = format!("source:{}", context.client_key());
    if let Some(replayed) = idempotency::lookup(
        &state,
        &context,
        &idempotency_scope,
        "POST",
        "/v1/identities",
        &body,
    )
    .await?
    {
        return Ok(replayed);
    }

    // Registrations per source are bounded (spec §10).
    let source = context.client_key();
    if snapshot.bool(keys::RATELIMIT_ENABLED) {
        let limit = snapshot.int(keys::RATELIMIT_IDENTITY_REGISTRATIONS_PER_HOUR_PER_SOURCE);
        let since = now() - chrono::Duration::hours(1);
        let source_for_count = source.clone();
        let db = state.db.clone();
        let count = db
            .call(move |conn| repo::count_registrations_since(conn, &source_for_count, since))
            .await?;
        if count >= limit {
            metrics::RATE_LIMITED_REQUESTS.inc();
            return Err(ApiError::rate_limited(
                snapshot.u64(keys::RATELIMIT_RETRY_AFTER_SECONDS),
            ));
        }
    }

    let record = verify_challenge(
        &state,
        &snapshot,
        &request.challenge_id,
        &request.signature,
        &[Operation::RegisterIdentity],
    )
    .await?;

    let key = record.key.clone();
    let source_for_record = source.clone();
    let db = state.db.clone();
    let (identity, created) = db
        .call(move |conn| {
            let result = in_txn(conn, |txn| repo::upsert_identity(txn, &key))?;
            if result.1 {
                repo::record_registration(conn, &source_for_record)?;
            }
            Ok(result)
        })
        .await?;

    tracing::info!(
        event = "identity_registered",
        request_id = %context.request_id,
        identity_id = %identity.identity_id,
        created,
        "identity registration completed"
    );

    let body_json = json!({
        "identity_id": identity.identity_id,
        "created_at": to_rfc3339(identity.created_at),
    });
    // Re-registering the same key is idempotent and answers 200 (spec §5.1).
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    idempotency::store(
        &state,
        &context,
        &idempotency_scope,
        "POST",
        "/v1/identities",
        &body,
        status,
        &body_json,
    )
    .await?;

    Ok((status, Json(body_json)).into_response())
}

// ------------------------------------------------------------------------ tokens

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub scopes: Vec<String>,
    pub principal: &'static str,
    pub principal_id: String,
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    JsonBody(request): JsonBody<SignedRequest>,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);

    let record = verify_challenge(
        &state,
        &snapshot,
        &request.challenge_id,
        &request.signature,
        &[
            Operation::AuthenticateIdentity,
            Operation::AuthenticateDevice,
        ],
    )
    .await?;

    if snapshot.bool(keys::RATELIMIT_ENABLED)
        && !state.rate_limiter.check(
            "token_fingerprint",
            &record.key_fingerprint,
            snapshot.int(keys::RATELIMIT_TOKEN_REQUESTS_PER_MINUTE_PER_FINGERPRINT),
        )
    {
        metrics::RATE_LIMITED_REQUESTS.inc();
        return Err(ApiError::rate_limited(
            snapshot.u64(keys::RATELIMIT_RETRY_AFTER_SECONDS),
        ));
    }

    let (kind, principal_id, identity_id, scopes) = match record.operation {
        Operation::AuthenticateIdentity => {
            let identity_id = record.key.fingerprint();
            let lookup = identity_id.clone();
            let db = state.db.clone();
            if !db
                .call(move |conn| repo::identity_exists(conn, &lookup))
                .await?
            {
                metrics::AUTH_FAILURES.inc();
                return Err(ApiError::unauthorized(
                    "this key is not a registered identity",
                ));
            }
            (
                PrincipalKind::Identity,
                identity_id.clone(),
                identity_id,
                snapshot.list(keys::AUTH_IDENTITY_TOKEN_SCOPES),
            )
        }
        Operation::AuthenticateDevice => {
            let fingerprint = record.key_fingerprint.clone();
            let db = state.db.clone();
            let device = db
                .call(move |conn| repo::get_device_by_fingerprint(conn, &fingerprint))
                .await?
                .ok_or_else(|| {
                    metrics::AUTH_FAILURES.inc();
                    ApiError::unauthorized("this key is not a registered device")
                })?;
            if device.revoked_at.is_some() {
                metrics::AUTH_FAILURES.inc();
                return Err(ApiError::unauthorized("this device has been revoked")
                    .with_code(code::DEVICE_REVOKED));
            }
            let device_id = device.device_id;
            let db = state.db.clone();
            let _ = db
                .call(move |conn| repo::touch_device(conn, device_id))
                .await;
            // Scopes follow the device's role, so a publisher can never receive
            // mirror or input authority and a client can never publish (spec §4.3).
            let scopes = match device.role {
                DeviceRole::Publisher => snapshot.list(keys::AUTH_DEVICE_TOKEN_SCOPES),
                DeviceRole::Client => snapshot.list(keys::AUTH_CLIENT_TOKEN_SCOPES),
                DeviceRole::Both => {
                    let mut scopes = snapshot.list(keys::AUTH_DEVICE_TOKEN_SCOPES);
                    for scope in snapshot.list(keys::AUTH_CLIENT_TOKEN_SCOPES) {
                        if !scopes.contains(&scope) {
                            scopes.push(scope);
                        }
                    }
                    scopes
                }
            };
            (
                PrincipalKind::Device,
                device.device_id.to_string(),
                device.identity_id,
                scopes,
            )
        }
        other => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                code::CHALLENGE_INVALID,
                format!("challenge was issued for {}", other.as_str()),
            ));
        }
    };

    let claims = build_claims(&snapshot, kind, &principal_id, &identity_id, scopes.clone());
    let expires_in = claims.exp - claims.iat;
    let access_token = state.tokens.mint(&claims)?;

    tracing::info!(
        event = "token_issued",
        request_id = %context.request_id,
        principal = %principal_id,
        principal_kind = kind.as_str(),
        "issued an access token"
    );

    Ok((
        StatusCode::OK,
        Json(TokenResponse {
            access_token,
            token_type: "Bearer",
            expires_in,
            scopes,
            principal: kind.as_str(),
            principal_id,
        }),
    )
        .into_response())
}

// ------------------------------------------------------------- websocket tickets

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketRequest {
    /// The exact path the ticket may be used for.
    pub path: String,
}

pub async fn create_websocket_ticket(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    JsonBody(request): JsonBody<TicketRequest>,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    let path = request.path.trim().to_string();

    // A ticket is valid for one specific mirror or device-relay path (spec §5.1), and
    // is only issued when the caller is already authorised for that path.
    let authorised = match parse_ws_path(&path) {
        Some(WsPath::Relay(device_id)) => {
            auth.require_scope(scope::TERMINALS_PUBLISH).is_ok()
                && auth.kind == PrincipalKind::Device
                && auth.principal_id == device_id.to_string()
        }
        Some(WsPath::Mirror(terminal_id)) => {
            // An identity, or a client-role device of that identity, may mirror.
            if auth.require_scope(scope::TERMINALS_MIRROR).is_err() {
                false
            } else {
                let db = state.db.clone();
                let terminal = db
                    .call(move |conn| repo::get_terminal(conn, terminal_id))
                    .await?;
                terminal
                    .map(|t| t.identity_id == auth.identity_id)
                    .unwrap_or(false)
            }
        }
        None => {
            return Err(ApiError::validation(
                "path must be /v1/devices/{device_id}/relay or /v1/terminals/{terminal_id}/mirror",
            ));
        }
    };

    if !authorised {
        // Same treatment as any other ownership failure (spec §4.4).
        return Err(ApiError::not_found());
    }

    let (ticket, ticket_hash) = new_ticket();
    let expires_at =
        now() + chrono::Duration::seconds(snapshot.int(keys::AUTH_WEBSOCKET_TICKET_TTL_SECONDS));

    let db = state.db.clone();
    let stored_path = path.clone();
    let kind = auth.kind;
    let principal_id = auth.principal_id.clone();
    let identity_id = auth.identity_id.clone();
    let scopes = auth.scopes.clone();
    db.call(move |conn| {
        repo::insert_ticket(
            conn,
            &ticket_hash,
            kind,
            &principal_id,
            &identity_id,
            &stored_path,
            &scopes,
            expires_at,
        )
    })
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ticket": ticket,
            "path": path,
            "expires_at": to_rfc3339(expires_at),
        })),
    )
        .into_response())
}

pub enum WsPath {
    Relay(Uuid),
    Mirror(Uuid),
}

pub fn parse_ws_path(path: &str) -> Option<WsPath> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["v1", "devices", id, "relay"] => Uuid::parse_str(id).ok().map(WsPath::Relay),
        ["v1", "terminals", id, "mirror"] => Uuid::parse_str(id).ok().map(WsPath::Mirror),
        _ => None,
    }
}

/// Authenticate a WebSocket upgrade from either a bearer token, a session cookie, or
/// a single-use ticket. Token material is never read from the query string.
pub async fn authenticate_upgrade(
    state: &Arc<AppState>,
    snapshot: &Snapshot,
    headers: &axum::http::HeaderMap,
    path: &str,
) -> ApiResult<Auth> {
    // Ticket first: it is scoped to exactly this path and is consumed on use.
    if let Some(raw) = headers
        .get("x-relay-ticket")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| cookie_value(headers, "relay_ticket"))
    {
        let hash = hash_ticket(&raw);
        let path_owned = path.to_string();
        let db = state.db.clone();
        let record = db
            .call(move |conn| in_txn(conn, |txn| repo::consume_ticket(txn, &hash, &path_owned)))
            .await?;
        let record = record.ok_or_else(|| {
            metrics::AUTH_FAILURES.inc();
            ApiError::unauthorized(
                "websocket ticket is invalid, expired, already used, or not valid for this path",
            )
        })?;
        metrics::AUTH_SUCCESSES.inc();
        return Ok(Auth {
            kind: record.principal_kind,
            principal_id: record.principal_id,
            identity_id: record.identity_id,
            scopes: record.scopes,
            jti: String::new(),
        });
    }

    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|v| v.trim().to_string())
        .or_else(|| cookie_value(headers, "relay_session"))
        .ok_or_else(|| {
            metrics::AUTH_FAILURES.inc();
            ApiError::unauthorized(
                "websocket upgrade requires a bearer token, session cookie, or ticket",
            )
        })?;

    let claims = state.tokens.verify(&token, snapshot).inspect_err(|_e| {
        metrics::AUTH_FAILURES.inc();
    })?;

    let principal_id = claims.sub.clone();
    let issued_at = claims.iat;
    let kind = claims.principal;
    let db = state.db.clone();
    let revoked = db
        .call(move |conn| {
            if let Some(cutoff) = repo::token_cutoff(conn, &principal_id)?
                && cutoff.timestamp() >= issued_at
            {
                return Ok(true);
            }
            if kind == PrincipalKind::Device {
                let device_id = Uuid::parse_str(&principal_id).unwrap_or_default();
                return Ok(repo::get_device(conn, device_id)?
                    .map(|d| d.revoked_at.is_some())
                    .unwrap_or(true));
            }
            Ok(false)
        })
        .await?;
    if revoked {
        metrics::AUTH_FAILURES.inc();
        return Err(
            ApiError::unauthorized("credential has been revoked").with_code(code::DEVICE_REVOKED)
        );
    }

    metrics::AUTH_SUCCESSES.inc();
    Ok(Auth {
        kind: claims.principal,
        principal_id: claims.sub,
        identity_id: claims.identity_id,
        scopes: claims.scopes,
        jti: claims.jti,
    })
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_paths_parse() {
        let id = Uuid::new_v4();
        assert!(matches!(
            parse_ws_path(&format!("/v1/devices/{id}/relay")),
            Some(WsPath::Relay(parsed)) if parsed == id
        ));
        assert!(matches!(
            parse_ws_path(&format!("/v1/terminals/{id}/mirror")),
            Some(WsPath::Mirror(parsed)) if parsed == id
        ));
        assert!(parse_ws_path("/v1/terminals/not-a-uuid/mirror").is_none());
        assert!(parse_ws_path("/v1/other").is_none());
    }

    #[test]
    fn cookies_are_parsed_by_name() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            "other=1; relay_session=abc123; trailing=2".parse().unwrap(),
        );
        assert_eq!(
            cookie_value(&headers, "relay_session").as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie_value(&headers, "missing"), None);
    }
}
