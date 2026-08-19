//! Persistence, seeding, atomic updates and propagation of runtime settings.

use super::defs::{DEFS, SETTINGS_SCHEMA_VERSION, keys};
use super::{Snapshot, Value, find_def, validate_combination, validate_value};
use crate::bootstrap::Bootstrap;
use crate::crypto::SecretBox;
use crate::db::{Db, in_txn};
use crate::error::{ApiError, ApiResult, code};
use crate::util::{b64_encode, now, random_bytes, to_rfc3339};
use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::watch;

const META_SETTINGS_SCHEMA_VERSION: &str = "settings_schema_version";

pub struct SettingsStore {
    db: Db,
    secret_box: Arc<SecretBox>,
    tx: watch::Sender<Arc<Snapshot>>,
}

/// Outcome of an accepted update, for the operator response and logging.
pub struct PatchOutcome {
    pub snapshot: Arc<Snapshot>,
    pub changed: Vec<String>,
}

impl SettingsStore {
    /// Seed defaults on first initialisation, then load and validate a complete
    /// snapshot. After seeding, the database is authoritative (spec §8.1).
    pub fn initialize(db: &Db, bootstrap: &Bootstrap) -> ApiResult<Arc<Self>> {
        let secret_box = Arc::new(SecretBox::new(bootstrap.secret_key));

        let mut conn = db.conn()?;
        let seed_token = bootstrap.operator_token_seed.clone();
        let data_dir = bootstrap.data_dir.clone();
        let sb = Arc::clone(&secret_box);

        in_txn(&mut conn, move |txn| {
            let stored_version: Option<String> = txn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key = ?1",
                    params![META_SETTINGS_SCHEMA_VERSION],
                    |row| row.get(0),
                )
                .optional()?;

            match stored_version {
                None => {
                    txn.execute(
                        "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)",
                        params![
                            META_SETTINGS_SCHEMA_VERSION,
                            SETTINGS_SCHEMA_VERSION.to_string()
                        ],
                    )?;
                }
                Some(v) => {
                    let parsed: i64 = v.parse().unwrap_or(-1);
                    if parsed != SETTINGS_SCHEMA_VERSION {
                        // Readiness must fail rather than silently reinterpret rows
                        // written by a different schema version (spec §8.1).
                        return Err(ApiError::internal(format!(
                            "database settings schema version {parsed} is not supported by this build (expected {SETTINGS_SCHEMA_VERSION})"
                        )));
                    }
                }
            }

            let timestamp = to_rfc3339(now());
            for def in DEFS {
                let exists: Option<String> = txn
                    .query_row(
                        "SELECT value_json FROM settings WHERE name = ?1",
                        params![def.name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    txn.execute(
                        "INSERT INTO settings (name, value_json, updated_at) VALUES (?1, ?2, ?3)",
                        params![
                            def.name,
                            def.default_value().to_json().to_string(),
                            timestamp
                        ],
                    )?;
                }
            }

            // Remove rows for settings this build no longer declares, so the stored
            // set matches the schema version exactly.
            let declared: Vec<String> = DEFS.iter().map(|d| d.name.to_string()).collect();
            let mut stale = Vec::new();
            {
                let mut stmt = txn.prepare("SELECT name FROM settings")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                for row in rows {
                    let name = row?;
                    if !declared.contains(&name) {
                        stale.push(name);
                    }
                }
            }
            for name in stale {
                tracing::warn!(event = "setting_removed", setting = %name, "dropping setting not declared by this build");
                txn.execute("DELETE FROM settings WHERE name = ?1", params![name])?;
            }

            seed_operator_token(txn, &sb, seed_token.as_deref(), &data_dir)?;

            txn.execute(
                "INSERT INTO settings_state (id, revision, updated_at) VALUES (1, 1, ?1)
                 ON CONFLICT (id) DO NOTHING",
                params![timestamp],
            )?;
            Ok(())
        })?;

        let snapshot = Arc::new(load(&conn, Arc::clone(&secret_box))?);
        let (tx, _rx) = watch::channel(snapshot);
        Ok(Arc::new(Self {
            db: db.clone(),
            secret_box,
            tx,
        }))
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }

    pub fn secret_box(&self) -> Arc<SecretBox> {
        Arc::clone(&self.secret_box)
    }

    /// Re-read committed state and publish it if the revision advanced.
    pub fn reload(&self) -> ApiResult<Arc<Snapshot>> {
        let conn = self.db.conn()?;
        let fresh = Arc::new(load(&conn, Arc::clone(&self.secret_box))?);
        if fresh.revision != self.tx.borrow().revision {
            tracing::info!(
                event = "settings_revision_applied",
                revision = fresh.revision,
                "applied settings revision"
            );
            // send_replace, not send: `send` drops the update when no receiver
            // exists yet, which would leave this instance serving a stale snapshot
            // after a revision it committed itself.
            self.tx.send_replace(Arc::clone(&fresh));
        }
        Ok(fresh)
    }

    /// Atomically apply an update set.
    ///
    /// Either every change commits with one new revision and audit entries, or
    /// nothing is applied (spec §5.5).
    pub fn patch(
        &self,
        operator: &str,
        expected_revision: i64,
        updates: BTreeMap<String, serde_json::Value>,
    ) -> ApiResult<PatchOutcome> {
        if updates.is_empty() {
            return Err(ApiError::invalid("no settings supplied"));
        }

        let mut conn = self.db.conn()?;
        let secret_box = Arc::clone(&self.secret_box);
        let operator = operator.to_string();

        let result = in_txn(&mut conn, |txn| {
            let current_revision: i64 = txn.query_row(
                "SELECT revision FROM settings_state WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            if current_revision != expected_revision {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    code::SETTINGS_REVISION_CONFLICT,
                    format!(
                        "settings revision {expected_revision} is stale; current revision is {current_revision}"
                    ),
                ));
            }

            let mut stored = read_rows(txn)?;
            let mut effective = resolve(&stored);

            let mut errors = Vec::new();
            let mut staged: Vec<(String, Value, Option<Value>, String)> = Vec::new();

            for (name, json) in &updates {
                let Some(def) = find_def(name) else {
                    errors.push(format!("unknown setting: {name}"));
                    continue;
                };
                let parsed = match Value::from_json(def, json) {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };
                if let Err(e) = validate_value(def, &parsed) {
                    errors.push(e);
                    continue;
                }

                // Secrets are stored either as an external reference or encrypted
                // under bootstrap key material (spec §8.1).
                let stored_value = if def.secret {
                    match &parsed {
                        Value::Str(s) if s.is_empty() => Value::Str(String::new()),
                        Value::Str(s)
                            if s.starts_with("env:")
                                || s.starts_with("file:")
                                || s.starts_with("enc:v1:") =>
                        {
                            Value::Str(s.clone())
                        }
                        Value::Str(s) => Value::Str(secret_box.seal_to_string(s)),
                        other => other.clone(),
                    }
                } else {
                    parsed.clone()
                };

                let previous = effective.get(name.as_str()).cloned();
                // Audit hashes are computed over plaintext for secrets so equal
                // values hash equally; raw values never reach the audit log.
                let new_hash = if def.secret {
                    hash_secret_for_audit(&parsed)
                } else {
                    parsed.audit_hash()
                };
                effective.insert(name.clone(), parsed.clone());
                stored.insert(name.clone(), stored_value.clone());
                staged.push((name.clone(), stored_value, previous, new_hash));
            }

            if errors.is_empty()
                && let Err(mut combination_errors) = validate_combination(&effective)
            {
                errors.append(&mut combination_errors);
            }

            if !errors.is_empty() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    code::SETTINGS_INVALID,
                    errors.join("; "),
                ));
            }

            let new_revision = current_revision + 1;
            let timestamp = to_rfc3339(now());
            let mut changed = Vec::new();

            for (name, stored_value, previous, new_hash) in staged {
                let def = find_def(&name).expect("validated above");
                let old_hash = previous.as_ref().map(|v| {
                    if def.secret {
                        hash_secret_for_audit(v)
                    } else {
                        v.audit_hash()
                    }
                });
                let unchanged = old_hash.as_deref() == Some(new_hash.as_str());

                txn.execute(
                    "UPDATE settings SET value_json = ?2, updated_at = ?3 WHERE name = ?1",
                    params![name, stored_value.to_json().to_string(), timestamp],
                )?;
                txn.execute(
                    "INSERT INTO settings_audit
                        (revision, at, operator, setting, old_value_hash, new_value_hash, outcome, detail)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'applied', ?7)",
                    params![
                        new_revision,
                        timestamp,
                        operator,
                        name,
                        old_hash,
                        new_hash,
                        if unchanged { "value unchanged" } else { "value changed" }
                    ],
                )?;
                if !unchanged {
                    changed.push(name);
                }
            }

            txn.execute(
                "UPDATE settings_state SET revision = ?1, updated_at = ?2 WHERE id = 1",
                params![new_revision, timestamp],
            )?;

            Ok(changed)
        });

        match result {
            Ok(changed) => {
                let fresh = Arc::new(load(&conn, Arc::clone(&self.secret_box))?);
                tracing::info!(
                    event = "settings_updated",
                    revision = fresh.revision,
                    operator = %operator,
                    changed = changed.len(),
                    "settings update committed"
                );
                // send_replace, not send: `send` drops the update when no receiver
                // exists yet, which would leave this instance serving a stale snapshot
                // after a revision it committed itself.
                self.tx.send_replace(Arc::clone(&fresh));
                Ok(PatchOutcome {
                    snapshot: fresh,
                    changed,
                })
            }
            Err(err) => {
                // Rejections are recorded in their own transaction, since the
                // rejected one rolled back.
                let names: Vec<String> = updates.keys().cloned().collect();
                if let Err(audit_err) = record_rejection(&self.db, &operator, &names, &err.message)
                {
                    tracing::error!(
                        event = "settings_audit_failed",
                        error = %audit_err,
                        "could not record rejected settings update"
                    );
                }
                Err(err)
            }
        }
    }

    /// Poll for revisions committed by another instance so all healthy instances
    /// converge within the configured propagation interval (spec §5.5).
    pub fn spawn_propagation_watcher(
        self: &Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let store = Arc::clone(self);
        let mut settings_rx = self.subscribe();
        tokio::spawn(async move {
            loop {
                let interval = store
                    .snapshot()
                    .duration_ms(keys::SETTINGS_PROPAGATION_INTERVAL_MS);
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    // A change to the propagation interval itself must be adopted at
                    // once, not after the previous interval elapses.
                    changed = settings_rx.changed() => {
                        if changed.is_err() { return; }
                        continue;
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return; }
                    }
                }

                let store_for_read = Arc::clone(&store);
                let committed = tokio::task::spawn_blocking(move || -> ApiResult<i64> {
                    let conn = store_for_read.db.conn()?;
                    let revision: i64 = conn.query_row(
                        "SELECT revision FROM settings_state WHERE id = 1",
                        [],
                        |row| row.get(0),
                    )?;
                    Ok(revision)
                })
                .await;

                let committed = match committed {
                    Ok(Ok(revision)) => revision,
                    Ok(Err(e)) => {
                        tracing::warn!(event = "settings_poll_failed", error = %e);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(event = "settings_poll_failed", error = %e);
                        continue;
                    }
                };

                if committed != store.snapshot().revision {
                    let store_for_reload = Arc::clone(&store);
                    let reloaded =
                        tokio::task::spawn_blocking(move || store_for_reload.reload()).await;
                    if let Ok(Err(e)) = reloaded {
                        // A revision that cannot be loaded is a hard problem: keep
                        // serving the last good snapshot and let readiness report it.
                        tracing::error!(
                            event = "settings_reload_failed",
                            error = %e,
                            "committed settings revision could not be applied"
                        );
                        crate::metrics::SETTINGS_RELOAD_FAILURES.inc();
                    }
                }
            }
        });
    }

    /// Used by readiness: confirm the committed revision still loads and validates.
    pub fn verify_loadable(&self) -> ApiResult<i64> {
        let conn = self.db.conn()?;
        let snapshot = load(&conn, Arc::clone(&self.secret_box))?;
        Ok(snapshot.revision)
    }
}

fn hash_secret_for_audit(value: &Value) -> String {
    match value {
        Value::Str(s) if s.is_empty() => crate::util::sha256_hex(b"unset"),
        Value::Str(s) => crate::util::sha256_hex(format!("secret:{s}").as_bytes()),
        other => other.audit_hash(),
    }
}

fn record_rejection(db: &Db, operator: &str, names: &[String], detail: &str) -> ApiResult<()> {
    let conn = db.conn()?;
    let revision: i64 = conn.query_row(
        "SELECT revision FROM settings_state WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    let timestamp = to_rfc3339(now());
    for name in names {
        conn.execute(
            "INSERT INTO settings_audit
                (revision, at, operator, setting, old_value_hash, new_value_hash, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, 'rejected', ?5)",
            params![revision, timestamp, operator, name, detail],
        )?;
    }
    Ok(())
}

fn read_rows(conn: &Connection) -> ApiResult<BTreeMap<String, Value>> {
    let mut stmt = conn.prepare("SELECT name, value_json FROM settings")?;
    let mut out = BTreeMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (name, raw) = row?;
        let Some(def) = find_def(&name) else { continue };
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            ApiError::internal(format!("stored setting {name} is not valid JSON: {e}"))
        })?;
        let value = Value::from_json(def, &json)
            .map_err(|e| ApiError::internal(format!("stored setting is invalid: {e}")))?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Fill in any absent key from its declared default.
fn resolve(stored: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for def in DEFS {
        let value = stored
            .get(def.name)
            .cloned()
            .unwrap_or_else(|| def.default_value());
        out.insert(def.name.to_string(), value);
    }
    out
}

/// Load and fully validate a snapshot. Any invalid stored value is an error, so
/// readiness fails rather than falling back to a compiled default (spec §8.1).
pub fn load(conn: &Connection, secret_box: Arc<SecretBox>) -> ApiResult<Snapshot> {
    let revision: i64 = conn
        .query_row(
            "SELECT revision FROM settings_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| ApiError::internal(format!("cannot read settings revision: {e}")))?;

    let stored = read_rows(conn)?;
    let values = resolve(&stored);

    for def in DEFS {
        let value = values.get(def.name).expect("resolve fills every key");
        validate_value(def, value).map_err(|e| {
            ApiError::internal(format!("stored setting is invalid, refusing to serve: {e}"))
        })?;
    }
    validate_combination(&values).map_err(|errors| {
        ApiError::internal(format!(
            "stored settings form an invalid combination, refusing to serve: {}",
            errors.join("; ")
        ))
    })?;

    Ok(Snapshot::new(revision, values, secret_box))
}

/// Establish an operator credential on first initialisation.
///
/// The token itself is written to a 0600 file in the data directory rather than to
/// the log, so no credential lands in log storage.
fn seed_operator_token(
    txn: &rusqlite::Transaction<'_>,
    secret_box: &SecretBox,
    seed: Option<&str>,
    data_dir: &std::path::Path,
) -> ApiResult<()> {
    let current: Option<String> = txn
        .query_row(
            "SELECT value_json FROM settings WHERE name = ?1",
            params![keys::AUTH_OPERATOR_TOKEN_HASH],
            |row| row.get(0),
        )
        .optional()?;

    let already_set = current
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| v.as_str().map(|s| !s.is_empty()))
        .unwrap_or(false);
    if already_set {
        return Ok(());
    }

    let (token, generated) = match seed {
        Some(t) => (t.to_string(), false),
        None => (b64_encode(&random_bytes(32)), true),
    };
    let hash = crate::crypto::hash_operator_token(&token);
    let stored = secret_box.seal_to_string(&hash);

    txn.execute(
        "UPDATE settings SET value_json = ?2, updated_at = ?3 WHERE name = ?1",
        params![
            keys::AUTH_OPERATOR_TOKEN_HASH,
            serde_json::Value::String(stored).to_string(),
            to_rfc3339(now())
        ],
    )?;

    if generated {
        let path = data_dir.join("operator-token");
        if let Err(e) = std::fs::write(&path, format!("{token}\n")) {
            tracing::error!(
                event = "operator_token_write_failed",
                path = %path.display(),
                error = %e,
                "generated an operator token but could not write it; set auth.operator_token_hash via RELAY_OPERATOR_TOKEN"
            );
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            tracing::warn!(
                event = "operator_token_generated",
                path = %path.display(),
                "generated an operator token; read it from this file and then remove the file"
            );
        }
    }

    Ok(())
}
