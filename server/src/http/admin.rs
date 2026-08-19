//! Operator-only settings API (spec §5.5).

use super::auth::OperatorAuth;
use super::context::RequestContext;
use super::extract::JsonBody;
use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::relay::flush;
use crate::settings::{DefaultValue, Kind, SettingDef, defs::DEFS};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value as Json_, json};
use std::collections::BTreeMap;
use std::sync::Arc;

fn default_json(def: &SettingDef) -> Json_ {
    match def.default {
        DefaultValue::Bool(b) => Json_::Bool(b),
        DefaultValue::Int(i) => Json_::from(i),
        DefaultValue::Str(s) => Json_::String(s.to_string()),
        DefaultValue::List(l) => {
            Json_::Array(l.iter().map(|s| Json_::String(s.to_string())).collect())
        }
    }
}

/// Describe one setting, with its value redacted when it is a secret.
fn setting_json(def: &SettingDef, snapshot: &crate::settings::Snapshot) -> Json_ {
    let mut value = json!({
        "name": def.name,
        "type": def.kind,
        "description": def.description,
        "secret": def.secret,
        "reload": def.reload,
        "default": default_json(def),
    });

    if def.secret {
        // Only whether, and how, a secret is configured (spec §5.5).
        value["value"] = Json_::Null;
        value["configured"] = Json_::Bool(snapshot.secret_form(def.name) != "unset");
        value["secret_form"] = Json_::String(snapshot.secret_form(def.name).to_string());
    } else {
        value["value"] = snapshot
            .values
            .get(def.name)
            .map(|v| v.to_json())
            .unwrap_or_else(|| default_json(def));
    }

    if let Some(min) = def.min {
        value["minimum"] = Json_::from(min);
    }
    if let Some(max) = def.max {
        value["maximum"] = Json_::from(max);
    }
    if let Some(allowed) = def.allowed {
        value["allowed"] = Json_::Array(
            allowed
                .iter()
                .map(|v| Json_::String((*v).to_string()))
                .collect(),
        );
    }
    if matches!(def.kind, Kind::Int) {
        value["unit_hint"] = Json_::String(unit_hint(def.name).to_string());
    }
    value
}

fn unit_hint(name: &str) -> &'static str {
    if name.ends_with("_bytes") {
        "bytes"
    } else if name.ends_with("_ms") {
        "milliseconds"
    } else if name.ends_with("_seconds") {
        "seconds"
    } else if name.ends_with("_pages") {
        "pages"
    } else if name.ends_with("_kib") {
        "kibibytes"
    } else {
        "count"
    }
}

pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    _operator: OperatorAuth,
) -> ApiResult<Response> {
    let snapshot = Arc::clone(&context.snapshot);
    let settings: Vec<Json_> = DEFS
        .iter()
        .map(|def| setting_json(def, &snapshot))
        .collect();

    // Report the committed revision too, so an operator can see propagation lag.
    let db = state.db.clone();
    let committed = db
        .call(|conn| {
            Ok(conn.query_row(
                "SELECT revision FROM settings_state WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap_or(snapshot.revision);

    Ok(Json(json!({
        "revision": snapshot.revision,
        "committed_revision": committed,
        "schema_version": crate::settings::defs::SETTINGS_SCHEMA_VERSION,
        "instance_id": state.bootstrap.instance_id,
        "settings": settings,
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    /// The revision the operator read. A stale value is a conflict.
    pub revision: i64,
    pub settings: BTreeMap<String, Json_>,
}

pub async fn patch_settings(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    operator: OperatorAuth,
    JsonBody(request): JsonBody<PatchRequest>,
) -> ApiResult<Response> {
    let settings_store = Arc::clone(&state.settings);
    let operator_principal = operator.principal.clone();
    let revision = request.revision;
    let updates = request.settings;

    let outcome = tokio::task::spawn_blocking(move || {
        settings_store.patch(&operator_principal, revision, updates)
    })
    .await
    .map_err(|e| ApiError::internal(format!("settings update task failed: {e}")))??;

    tracing::info!(
        event = "settings_patch_applied",
        request_id = %context.request_id,
        operator = %operator.principal,
        revision = outcome.snapshot.revision,
        changed = outcome.changed.len(),
        "settings update applied"
    );

    let snapshot = outcome.snapshot;
    let settings: Vec<Json_> = DEFS
        .iter()
        .map(|def| setting_json(def, &snapshot))
        .collect();

    Ok(Json(json!({
        "revision": snapshot.revision,
        "changed": outcome.changed,
        "settings": settings,
    }))
    .into_response())
}

/// An explicit operator flush, one of the checkpoint triggers the specification
/// requires (spec §7.2).
pub async fn flush_now(
    State(state): State<Arc<AppState>>,
    context: RequestContext,
    operator: OperatorAuth,
) -> ApiResult<Response> {
    let bytes = flush::flush_once(&state.registry, flush::FlushTrigger::Requested).await;
    tracing::info!(
        event = "operator_flush",
        request_id = %context.request_id,
        operator = %operator.principal,
        bytes,
        "operator requested a checkpoint"
    );
    Ok(Json(json!({
        "flushed_bytes": bytes,
        "storage_healthy": !state.registry.storage_failing(),
    }))
    .into_response())
}

/// Recent settings audit entries, so an operator can review changes in place.
pub async fn get_audit(
    State(state): State<Arc<AppState>>,
    _context: RequestContext,
    _operator: OperatorAuth,
) -> ApiResult<Response> {
    let db = state.db.clone();
    let entries = db
        .call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT revision, at, operator, setting, old_value_hash, new_value_hash, outcome, detail
                   FROM settings_audit ORDER BY id DESC LIMIT 200",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(json!({
                    "revision": row.get::<_, i64>(0)?,
                    "at": row.get::<_, String>(1)?,
                    "operator": row.get::<_, String>(2)?,
                    "setting": row.get::<_, String>(3)?,
                    "old_value_hash": row.get::<_, Option<String>>(4)?,
                    "new_value_hash": row.get::<_, Option<String>>(5)?,
                    "outcome": row.get::<_, String>(6)?,
                    "detail": row.get::<_, Option<String>>(7)?,
                }))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?;

    Ok(Json(json!({ "entries": entries })).into_response())
}
