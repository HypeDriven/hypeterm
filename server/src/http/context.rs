//! Per-request context: correlation ID, client address, transport security, and the
//! settings snapshot the whole request will use.

use crate::app::AppState;
use crate::error::{ApiError, ApiResult, REQUEST_ID, code};
use crate::metrics;
use crate::settings::defs::keys;
use crate::settings::{Snapshot, cidr_contains, parse_cidr};
use crate::util::new_ulid;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderValue, StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Connection-level facts injected by the listener, which knows the peer address and
/// whether TLS was terminated in-process.
#[derive(Clone, Copy, Debug)]
pub struct ConnMeta {
    pub peer: SocketAddr,
    pub tls: bool,
}

/// Lets the auth extractor report the authenticated principal back to the access log.
#[derive(Clone, Default)]
pub struct PrincipalLog(Arc<Mutex<Option<String>>>);

impl PrincipalLog {
    pub fn set(&self, principal: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(principal.to_string());
    }

    fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub client_ip: IpAddr,
    pub secure: bool,
    pub snapshot: Arc<Snapshot>,
    /// Value of the `Idempotency-Key` header, when the feature is enabled.
    pub idempotency_key: Option<String>,
}

impl RequestContext {
    pub fn client_key(&self) -> String {
        self.client_ip.to_string()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestContext {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestContext>()
            .cloned()
            .ok_or_else(|| ApiError::internal("request context is missing"))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PrincipalLog {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<PrincipalLog>()
            .cloned()
            .unwrap_or_default())
    }
}

/// Resolve the effective client address, honouring forwarded headers only when the
/// immediate peer is a configured trusted proxy (spec §8.1).
fn resolve_client(
    parts_headers: &axum::http::HeaderMap,
    conn: Option<ConnMeta>,
    snapshot: &Snapshot,
) -> (IpAddr, bool) {
    let peer_ip = conn
        .map(|c| c.peer.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let tls = conn.map(|c| c.tls).unwrap_or(false);

    if !snapshot.bool(keys::SECURITY_TRUSTED_PROXY_ENABLED) {
        return (peer_ip, tls);
    }

    let trusted = in_networks(
        &snapshot.list(keys::SECURITY_TRUSTED_PROXY_NETWORKS),
        peer_ip,
    );
    if !trusted {
        return (peer_ip, tls);
    }

    let forwarded_for_header = snapshot.string(keys::SECURITY_FORWARDED_FOR_HEADER);
    let forwarded_proto_header = snapshot.string(keys::SECURITY_FORWARDED_PROTO_HEADER);

    let client_ip = parts_headers
        .get(&forwarded_for_header)
        .and_then(|v| v.to_str().ok())
        // The left-most entry is the original client.
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer_ip);

    let secure = tls
        || parts_headers
            .get(&forwarded_proto_header)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("https")
            })
            .unwrap_or(false);

    (client_ip, secure)
}

/// The main-listener middleware: correlation ID, transport security, body limit,
/// request timeout, access log and metrics.
pub async fn middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let snapshot = state.snapshot();
    let request_id = new_ulid();
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let conn = request.extensions().get::<ConnMeta>().copied();
    let (client_ip, secure) = resolve_client(request.headers(), conn, &snapshot);

    let idempotency_key =
        super::idempotency::extract(request.headers(), snapshot.bool(keys::IDEMPOTENCY_ENABLED));

    let context = RequestContext {
        request_id: request_id.clone(),
        client_ip,
        secure,
        snapshot: Arc::clone(&snapshot),
        idempotency_key,
    };
    let principal_log = PrincipalLog::default();
    request.extensions_mut().insert(context.clone());
    request.extensions_mut().insert(principal_log.clone());

    let outcome = REQUEST_ID
        .scope(request_id.clone(), async move {
            if let Err(e) = enforce_transport(&context, client_ip) {
                return e.into_response();
            }
            if let Err(e) = enforce_body_limit(&request, &snapshot) {
                return e.into_response();
            }

            let timeout = snapshot.duration_secs(keys::SERVER_REQUEST_TIMEOUT_SECONDS);
            // WebSocket upgrades run for the lifetime of the connection, so the
            // request timeout must not apply to them.
            let is_upgrade = request
                .headers()
                .get(axum::http::header::UPGRADE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false);

            if is_upgrade {
                next.run(request).await
            } else {
                match tokio::time::timeout(timeout, next.run(request)).await {
                    Ok(response) => response,
                    Err(_) => ApiError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        code::INTERNAL,
                        "request exceeded the configured timeout",
                    )
                    .into_response(),
                }
            }
        })
        .await;

    let mut response = outcome;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }

    let elapsed = started.elapsed();
    let status = response.status();
    metrics::HTTP_REQUESTS.inc();
    metrics::HTTP_REQUEST_SECONDS.observe(elapsed.as_secs_f64());
    if status.is_client_error() || status.is_server_error() {
        metrics::HTTP_REQUESTS_FAILED.inc();
    }

    tracing::info!(
        event = "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
        status = status.as_u16(),
        latency_ms = elapsed.as_millis() as u64,
        client = %client_ip,
        principal = principal_log.get().unwrap_or_else(|| "-".to_string()),
        "served an HTTP request"
    );

    response
}

/// Plain HTTP is refused in production, with a documented loopback exemption for
/// development (spec §4.1).
fn enforce_transport(context: &RequestContext, client_ip: IpAddr) -> ApiResult<()> {
    if context.secure {
        return Ok(());
    }
    if !context
        .snapshot
        .bool(keys::SECURITY_REQUIRE_SECURE_TRANSPORT)
    {
        return Ok(());
    }
    // A trusted terminator that forwards raw TCP sets no forwarded-protocol header, so
    // the only evidence the transport was secure is where the connection came from.
    // This is spec §4.1's "trusted TLS-terminating reverse proxy", identified by
    // address rather than by header.
    if in_networks(
        &context
            .snapshot
            .list(keys::SECURITY_TLS_TERMINATED_BY_NETWORKS),
        client_ip,
    ) {
        return Ok(());
    }
    if context
        .snapshot
        .bool(keys::SECURITY_ALLOW_INSECURE_LOOPBACK)
        && client_ip.is_loopback()
    {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        code::INSECURE_TRANSPORT,
        "this endpoint requires TLS; enable server.tls_enabled or run behind a trusted TLS-terminating proxy",
    ))
}

/// Whether an address falls inside any of the configured CIDR blocks.
fn in_networks(networks: &[String], candidate: IpAddr) -> bool {
    networks
        .iter()
        .filter_map(|cidr| parse_cidr(cidr))
        .any(|(network, prefix)| cidr_contains(network, prefix, candidate))
}

fn enforce_body_limit(request: &Request, snapshot: &Snapshot) -> ApiResult<()> {
    let limit = snapshot.u64(keys::LIMITS_MAX_REQUEST_BODY_BYTES);
    if let Some(length) = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && length > limit
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            code::PAYLOAD_TOO_LARGE,
            format!("request body of {length} bytes exceeds the {limit} byte limit"),
        ));
    }
    Ok(())
}
