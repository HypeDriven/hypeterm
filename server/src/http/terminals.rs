//! Read-only terminal resources for identity clients (spec §5.3).
//!
//! Terminal lifecycle is managed by the publisher over its relay WebSocket, so the
//! ordering between lifecycle and output events stays explicit; these endpoints only
//! report state.

use super::auth::Auth;
use super::context::RequestContext;
use super::devices::resolve_limit;
use super::extract::{Path, Query};
use super::idempotency;
use crate::app::AppState;
use crate::db::repo::{self, Cursor, TerminalFilters, TerminalRow, TerminalState};
use crate::error::{ApiError, ApiResult};
use crate::metrics;
use crate::relay::registry::{BeginOpen, OpenOutcome, PublisherDelivery};
use crate::relay::terminal::Offsets;
use crate::settings::defs::keys;
use crate::util::to_rfc3339;
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Offsets come from memory when the terminal is resident, because `next_offset`
/// advances as soon as bytes are accepted and only `durable_offset` is stored.
fn offsets_for(state: &Arc<AppState>, row: &TerminalRow) -> Offsets {
    match state.registry.resident(row.terminal_id) {
        Some(handle) => handle.offsets(),
        None => Offsets {
            earliest_offset: row.earliest_offset,
            next_offset: row.durable_offset,
            durable_offset: row.durable_offset,
        },
    }
}

fn terminal_json(state: &Arc<AppState>, row: &TerminalRow, detailed: bool) -> Value {
    let offsets = offsets_for(state, row);
    let live_state = match state.registry.resident(row.terminal_id) {
        Some(handle) => handle.lifecycle().as_str(),
        None => row.state.as_str(),
    };

    let mut value = json!({
        "terminal_id": row.terminal_id,
        "device_id": row.device_id,
        "identity_id": row.identity_id,
        "label": row.label,
        "local_ref": row.local_ref,
        "state": live_state,
        "cols": row.cols,
        "rows": row.rows,
        "term": row.term,
        "created_at": to_rfc3339(row.created_at),
        "last_activity_at": to_rfc3339(row.last_activity_at),
        "closed_at": row.closed_at.map(to_rfc3339),
        "close_reason": row.close_reason,
        "accepts_input": row.accepts_input,
        "earliest_offset": offsets.earliest_offset,
        "next_offset": offsets.next_offset,
        "durable_offset": offsets.durable_offset,
        "retained_bytes": offsets.retained_bytes(),
    });

    if detailed {
        if let Some(process_label) = &row.process_label {
            value["process_label"] = Value::String(process_label.clone());
        }
        value["replay_capacity_bytes"] = json!(state.snapshot().replay_capacity() as u64);
    }
    value
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub device_id: Option<Uuid>,
    #[serde(default)]
    pub state: Option<String>,
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
    // Deliberately not identity-only. Spec §4.4 lets a `client` or `both` device list
    // the terminals of the identity that owns it — that is how a paired phone
    // discovers what it may mirror, and why §4.3 grants such a device
    // `terminals:read` at all. The query below is scoped to `auth.identity_id`, which
    // a device token carries just as an identity token does, so widening this does not
    // widen what any principal can see.
    auth.require_scope("terminals:read")?;

    let limit = resolve_limit(query.limit, &context.snapshot)?;
    let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;

    let filter_state = match query.state.as_deref() {
        None => None,
        Some(raw) => Some(
            TerminalState::parse(raw)
                .ok_or_else(|| ApiError::validation("state must be open or closed"))?,
        ),
    };

    let filters = TerminalFilters {
        device_id: query.device_id,
        state: filter_state,
    };
    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    let mut rows = db
        .call(move |conn| {
            repo::list_terminals(conn, &identity_id, &filters, cursor.as_ref(), limit + 1)
        })
        .await?;

    let next_cursor = if rows.len() as i64 > limit {
        rows.truncate(limit as usize);
        rows.last()
            .map(|r| Cursor::encode(&to_rfc3339(r.created_at), &r.terminal_id.to_string()))
    } else {
        None
    };

    Ok(Json(json!({
        "terminals": rows.iter().map(|row| terminal_json(&state, row, false)).collect::<Vec<_>>(),
        "next_cursor": next_cursor,
    }))
    .into_response())
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    _context: RequestContext,
    auth: Auth,
    Path(terminal_id): Path<Uuid>,
) -> ApiResult<Response> {
    // As with `list`: a client device may read its own identity's terminals (§4.4).
    // The ownership check below is what enforces the boundary.
    auth.require_scope("terminals:read")?;

    let db = state.db.clone();
    let row = db
        .call(move |conn| repo::get_terminal(conn, terminal_id))
        .await?
        .ok_or_else(ApiError::not_found)?;

    if row.identity_id != auth.identity_id {
        return Err(ApiError::not_found());
    }

    Ok(Json(terminal_json(&state, &row, true)).into_response())
}

// ------------------------------------------- asking a device to open one (spec §4.6)

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTerminalRequest {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    cols: Option<u32>,
    #[serde(default)]
    rows: Option<u32>,
}

/// A label crosses into the machine owner's own terminal, where `hypeterm-publish list`
/// prints it, and onward across an argv boundary. Control characters are refused rather
/// than stripped: stripping would leave the phone showing one string and the laptop
/// another, which is exactly the confusion an injection wants.
fn validate_label(label: &str, max_bytes: usize) -> Result<(), String> {
    if label.len() > max_bytes {
        return Err(format!("label exceeds {max_bytes} bytes"));
    }
    if label.chars().all(char::is_whitespace) {
        return Err("label must not be blank".to_string());
    }
    if label
        .chars()
        .any(|c| c.is_control() || ('\u{80}'..='\u{9F}').contains(&c))
    {
        return Err("label must not contain control characters".to_string());
    }
    Ok(())
}

/// Asks a device's connected publisher to open a terminal (spec §4.6).
///
/// The relay never creates the terminal: it forwards, waits, and reports. The row comes
/// into existence only through the publisher's ordinary `terminal.open`, so the
/// lifecycle-versus-output ordering that §5.3 protects is unchanged.
pub async fn create_for_device(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    auth: Auth,
    Path(device_id): Path<Uuid>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    auth.require_scope(crate::crypto::scope::TERMINALS_CREATE)?;

    let snapshot = &context.snapshot;
    if !snapshot.bool(keys::FEATURES_TERMINAL_CREATE_ENABLED) {
        metrics::TERMINAL_OPEN_REQUESTS_REFUSED.inc();
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            crate::error::code::FEATURE_DISABLED,
            "terminal creation is disabled on this deployment",
        ));
    }

    // Required, not merely supported: this causes a process to be created on another
    // machine, and a retry must never make a second one (spec §5.2).
    if context.idempotency_key.is_none() {
        return Err(ApiError::idempotency_key_required(
            "Idempotency-Key is required when asking a device to open a terminal",
        ));
    }

    let path = format!("/v1/devices/{device_id}/terminals");
    if let Some(replayed) =
        idempotency::lookup(&state, &context, &auth.identity_id, "POST", &path, &body).await?
    {
        return Ok(replayed);
    }

    let request: CreateTerminalRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;

    if let Some(label) = request.label.as_deref() {
        validate_label(label, snapshot.usize(keys::LIMITS_MAX_LABEL_BYTES))
            .map_err(ApiError::validation)?;
    }
    for (name, value) in [("cols", request.cols), ("rows", request.rows)] {
        if let Some(value) = value
            && (value == 0 || value > 10_000)
        {
            return Err(ApiError::validation(format!("{name} is out of range")));
        }
    }

    let identity_id = auth.identity_id.clone();
    let db = state.db.clone();
    let device = db
        .call(move |conn| repo::get_owned_device(conn, &identity_id, device_id))
        .await?;
    // A device that is not owned, does not exist, or has been revoked answers 404 alike
    // (spec §4.4): a 403 would confirm the device exists to somebody who may not know.
    // `get_owned_device` does not filter revoked rows, so that check is explicit here.
    let Some(device) = device.filter(|d| d.revoked_at.is_none() && d.role.may_publish()) else {
        return Err(ApiError::not_found());
    };

    let per_principal = snapshot.int(keys::RATELIMIT_TERMINAL_CREATES_PER_HOUR_PER_PRINCIPAL);
    let per_device = snapshot.int(keys::RATELIMIT_TERMINAL_CREATES_PER_HOUR_PER_DEVICE);
    if snapshot.bool(keys::RATELIMIT_ENABLED)
        && (!state.rate_limiter.check_window(
            "terminal_create_principal",
            &auth.principal_id,
            per_principal,
            3600,
        ) || !state.rate_limiter.check_window(
            "terminal_create_device",
            &device.device_id.to_string(),
            per_device,
            3600,
        ))
    {
        metrics::TERMINAL_OPEN_REQUESTS_REFUSED.inc();
        return Err(ApiError::rate_limited(60));
    }

    // Condition 2 and 4 together: a connected publisher that asserted the capability on
    // *this* connection. Never inferred from the presence of a connection alone.
    if !state
        .registry
        .publisher_supports_open_request(device.device_id)
    {
        metrics::TERMINAL_OPEN_REQUESTS_UNAVAILABLE.inc();
        return Err(ApiError::publisher_unavailable(
            "that device is not currently accepting terminal-open requests",
        ));
    }

    // Derived from the idempotency key, so two concurrent retries land on the same
    // pending entry. The stored idempotency record is only written after success, so it
    // alone cannot make concurrent retries converge — this is what does.
    let request_id = idempotency::key_hash(
        &auth.identity_id,
        "POST",
        &path,
        context.idempotency_key.as_deref().unwrap_or_default(),
    );

    let mut owns_request = true;
    let mut receiver = match state.registry.begin_open_request(
        device.device_id,
        &request_id,
        &auth.principal_id,
        snapshot.usize(keys::LIMITS_MAX_PENDING_OPEN_REQUESTS_PER_DEVICE),
        snapshot.usize(keys::LIMITS_MAX_PENDING_OPEN_REQUESTS_TOTAL),
    )? {
        BeginOpen::Fresh(receiver) => {
            let delivered = state.registry.deliver_to_publisher(
                device.device_id,
                PublisherDelivery::OpenRequestDelivery {
                    request_id: request_id.clone(),
                    label: request.label.clone(),
                    cols: request.cols,
                    rows: request.rows,
                },
            );
            if delivered.is_err() {
                // Registered before sending, so a failed send must retire the entry or
                // a later caller joins a request nobody is carrying.
                state.registry.resolve_open_request(
                    device.device_id,
                    &request_id,
                    OpenOutcome::Unavailable,
                );
                metrics::TERMINAL_OPEN_REQUESTS_UNAVAILABLE.inc();
                return Err(ApiError::publisher_unavailable(
                    "that device could not be reached",
                ));
            }
            receiver
        }
        // Already in flight for this key: wait on the same answer rather than asking
        // twice, which would spawn two shells for one request.
        BeginOpen::Joined(receiver) => {
            owns_request = false;
            receiver
        }
    };

    let wait = std::time::Duration::from_secs(
        snapshot.int(keys::TERMINAL_OPEN_REQUEST_TIMEOUT_SECONDS) as u64,
    );
    let outcome = tokio::time::timeout(wait, async {
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return Some(outcome);
            }
            if receiver.changed().await.is_err() {
                return None;
            }
        }
    })
    .await;

    let outcome = match outcome {
        Ok(Some(outcome)) => outcome,
        // Deliberately ambiguous: a terminal may still have been created. The caller is
        // told to re-ask under the same key rather than guess (spec §5.2).
        Ok(None) | Err(_) => {
            // Left in place on purpose: the answer may still arrive, and a retry under
            // the same key must be able to join it rather than ask again (spec §5.2).
            metrics::TERMINAL_OPEN_REQUESTS_TIMEOUT.inc();
            return Err(ApiError::publisher_timeout(
                "the device did not answer in time; retry with the same Idempotency-Key",
            ));
        }
    };

    let retire = |state: &Arc<AppState>| {
        if owns_request {
            state
                .registry
                .finish_open_request(device.device_id, &request_id);
        }
    };

    let (terminal_id, deduplicated) = match outcome {
        OpenOutcome::Opened {
            terminal_id,
            deduplicated,
        } => (terminal_id, deduplicated),
        OpenOutcome::Declined { reason } => {
            retire(&state);
            return Err(ApiError::publisher_declined(format!(
                "the device declined to open a terminal: {reason}"
            )));
        }
        OpenOutcome::Failed { code, message } => {
            retire(&state);
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                code,
                message,
            ));
        }
        OpenOutcome::Unavailable => {
            retire(&state);
            metrics::TERMINAL_OPEN_REQUESTS_UNAVAILABLE.inc();
            return Err(ApiError::publisher_unavailable(
                "that device disconnected before answering",
            ));
        }
    };

    let db = state.db.clone();
    let row = db
        .call(move |conn| repo::get_terminal(conn, terminal_id))
        .await?
        .ok_or_else(|| ApiError::internal("the opened terminal could not be read back"))?;

    let mut value = terminal_json(&state, &row, true);
    if let Value::Object(map) = &mut value {
        map.insert("deduplicated".to_string(), json!(deduplicated));
    }
    let status = if deduplicated {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::CREATED
    };
    metrics::TERMINAL_OPEN_REQUESTS_OPENED.inc();
    idempotency::store(
        &state,
        &context,
        &auth.identity_id,
        "POST",
        &path,
        &body,
        status,
        &value,
    )
    .await?;
    // Ordered: the record is durable before the pending entry goes, so a retry of this
    // key is never left with neither to find.
    retire(&state);

    let mut response = (status, Json(value)).into_response();
    if let Ok(location) = format!("/v1/terminals/{terminal_id}").parse() {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, location);
    }
    Ok(response)
}
