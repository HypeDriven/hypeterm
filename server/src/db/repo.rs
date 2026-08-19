//! Typed queries over durable state.

use crate::crypto::{DeviceRole, Operation, PrincipalKind, PublicKey};
use crate::error::{ApiError, ApiResult};
use crate::util::{b64_decode, b64_encode, now, parse_rfc3339, to_rfc3339};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

// -------------------------------------------------------------------- identities

pub struct Identity {
    pub identity_id: String,
    pub created_at: DateTime<Utc>,
}

/// Register a key, or return the existing identity for it.
///
/// Registering the same canonical public key is idempotent and yields the same
/// identity ID (spec §3.1), which the `UNIQUE (algorithm, public_key)` index and the
/// deterministic fingerprint together guarantee.
pub fn upsert_identity(txn: &Transaction<'_>, key: &PublicKey) -> ApiResult<(Identity, bool)> {
    let identity_id = key.fingerprint();
    let existing: Option<String> = txn
        .query_row(
            "SELECT created_at FROM identities WHERE identity_id = ?1",
            params![identity_id],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(created_at) = existing {
        let created_at = parse_rfc3339(&created_at).unwrap_or_else(now);
        return Ok((
            Identity {
                identity_id,
                created_at,
            },
            false,
        ));
    }

    let created_at = now();
    txn.execute(
        "INSERT INTO identities (identity_id, algorithm, public_key, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            identity_id,
            key.algorithm,
            key.bytes,
            to_rfc3339(created_at)
        ],
    )?;
    Ok((
        Identity {
            identity_id,
            created_at,
        },
        true,
    ))
}

pub fn identity_exists(conn: &Connection, identity_id: &str) -> ApiResult<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM identities WHERE identity_id = ?1",
            params![identity_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

// ----------------------------------------------------------------------- devices

#[derive(Debug, Clone)]
pub struct Device {
    pub device_id: Uuid,
    pub identity_id: String,
    pub algorithm: String,
    pub public_key: Vec<u8>,
    pub key_fingerprint: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub role: DeviceRole,
}

impl Device {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let device_id: String = row.get(0)?;
        let created_at: String = row.get(6)?;
        let last_seen_at: Option<String> = row.get(7)?;
        let revoked_at: Option<String> = row.get(8)?;
        Ok(Self {
            device_id: Uuid::parse_str(&device_id).unwrap_or_default(),
            identity_id: row.get(1)?,
            algorithm: row.get(2)?,
            public_key: row.get(3)?,
            key_fingerprint: row.get(4)?,
            name: row.get(5)?,
            created_at: parse_rfc3339(&created_at).unwrap_or_else(now),
            last_seen_at: last_seen_at.as_deref().and_then(parse_rfc3339),
            revoked_at: revoked_at.as_deref().and_then(parse_rfc3339),
            // A row written before roles existed reads as the default, publisher.
            role: DeviceRole::parse(&row.get::<_, String>(9)?).unwrap_or_default(),
        })
    }
}

const DEVICE_COLUMNS: &str = "device_id, identity_id, algorithm, public_key, key_fingerprint, \
                              name, created_at, last_seen_at, revoked_at, role";

pub fn insert_device(
    txn: &Transaction<'_>,
    identity_id: &str,
    key: &PublicKey,
    name: &str,
    role: DeviceRole,
) -> ApiResult<Device> {
    let device_id = Uuid::new_v4();
    let created_at = now();
    let fingerprint = key.fingerprint();
    txn.execute(
        "INSERT INTO devices
            (device_id, identity_id, algorithm, public_key, key_fingerprint, name, role, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            device_id.to_string(),
            identity_id,
            key.algorithm,
            key.bytes,
            fingerprint,
            name,
            role.as_str(),
            to_rfc3339(created_at)
        ],
    )?;
    Ok(Device {
        device_id,
        identity_id: identity_id.to_string(),
        algorithm: key.algorithm.clone(),
        public_key: key.bytes.clone(),
        key_fingerprint: fingerprint,
        name: name.to_string(),
        created_at,
        last_seen_at: None,
        revoked_at: None,
        role,
    })
}

pub fn count_active_devices(conn: &Connection, identity_id: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE identity_id = ?1 AND revoked_at IS NULL",
        params![identity_id],
        |row| row.get(0),
    )?)
}

/// Fetch a device the caller owns. Callers turn `None` into 404 regardless of
/// whether the device exists but belongs to someone else (spec §4.4).
pub fn get_owned_device(
    conn: &Connection,
    identity_id: &str,
    device_id: Uuid,
) -> ApiResult<Option<Device>> {
    let sql =
        format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = ?1 AND identity_id = ?2");
    Ok(conn
        .query_row(
            &sql,
            params![device_id.to_string(), identity_id],
            Device::from_row,
        )
        .optional()?)
}

pub fn get_device(conn: &Connection, device_id: Uuid) -> ApiResult<Option<Device>> {
    let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = ?1");
    Ok(conn
        .query_row(&sql, params![device_id.to_string()], Device::from_row)
        .optional()?)
}

pub fn get_device_by_fingerprint(
    conn: &Connection,
    fingerprint: &str,
) -> ApiResult<Option<Device>> {
    let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE key_fingerprint = ?1");
    Ok(conn
        .query_row(&sql, params![fingerprint], Device::from_row)
        .optional()?)
}

pub fn list_devices(
    conn: &Connection,
    identity_id: &str,
    cursor: Option<&Cursor>,
    limit: i64,
) -> ApiResult<Vec<Device>> {
    let mut out = Vec::new();
    match cursor {
        Some(cursor) => {
            let sql = format!(
                "SELECT {DEVICE_COLUMNS} FROM devices
                 WHERE identity_id = ?1 AND revoked_at IS NULL
                   AND (created_at > ?2 OR (created_at = ?2 AND device_id > ?3))
                 ORDER BY created_at, device_id LIMIT ?4"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![identity_id, cursor.timestamp, cursor.id, limit],
                Device::from_row,
            )?;
            for row in rows {
                out.push(row?);
            }
        }
        None => {
            let sql = format!(
                "SELECT {DEVICE_COLUMNS} FROM devices
                 WHERE identity_id = ?1 AND revoked_at IS NULL
                 ORDER BY created_at, device_id LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![identity_id, limit], Device::from_row)?;
            for row in rows {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// Revoke a device and invalidate its already-issued tokens.
///
/// Idempotent: revoking twice keeps the original timestamp and still reports success.
pub fn revoke_device(txn: &Transaction<'_>, device_id: Uuid) -> ApiResult<bool> {
    let at = now();
    let changed = txn.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE device_id = ?1 AND revoked_at IS NULL",
        params![device_id.to_string(), to_rfc3339(at)],
    )?;
    // Any token issued at or before now must stop working, so a live token cannot
    // outlive revocation (spec §5.2).
    txn.execute(
        "INSERT INTO principal_token_cutoffs (principal_id, not_before) VALUES (?1, ?2)
         ON CONFLICT (principal_id) DO UPDATE SET not_before = excluded.not_before",
        params![device_id.to_string(), to_rfc3339(at)],
    )?;
    txn.execute(
        "DELETE FROM websocket_tickets WHERE principal_id = ?1 AND consumed_at IS NULL",
        params![device_id.to_string()],
    )?;
    Ok(changed > 0)
}

pub fn token_cutoff(conn: &Connection, principal_id: &str) -> ApiResult<Option<DateTime<Utc>>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT not_before FROM principal_token_cutoffs WHERE principal_id = ?1",
            params![principal_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.as_deref().and_then(parse_rfc3339))
}

pub fn touch_device(conn: &Connection, device_id: Uuid) -> ApiResult<()> {
    conn.execute(
        "UPDATE devices SET last_seen_at = ?2 WHERE device_id = ?1",
        params![device_id.to_string(), to_rfc3339(now())],
    )?;
    Ok(())
}

// --------------------------------------------------------------------- terminals

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Open,
    Closed,
}

impl TerminalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalState::Open => "open",
            TerminalState::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(TerminalState::Open),
            "closed" => Some(TerminalState::Closed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalRow {
    pub terminal_id: Uuid,
    pub device_id: Uuid,
    pub identity_id: String,
    pub label: String,
    pub local_ref: String,
    pub state: TerminalState,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub term: Option<String>,
    pub process_label: Option<String>,
    pub accepts_input: bool,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub close_reason: Option<String>,
    pub durable_offset: u64,
    pub earliest_offset: u64,
}

const TERMINAL_COLUMNS: &str = "terminal_id, device_id, identity_id, label, local_ref, state, \
                                cols, rows, term, process_label, created_at, last_activity_at, \
                                closed_at, close_reason, durable_offset, earliest_offset, \
                                accepts_input";

impl TerminalRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let terminal_id: String = row.get(0)?;
        let device_id: String = row.get(1)?;
        let state: String = row.get(5)?;
        let created_at: String = row.get(10)?;
        let last_activity_at: String = row.get(11)?;
        let closed_at: Option<String> = row.get(12)?;
        Ok(Self {
            terminal_id: Uuid::parse_str(&terminal_id).unwrap_or_default(),
            device_id: Uuid::parse_str(&device_id).unwrap_or_default(),
            identity_id: row.get(2)?,
            label: row.get(3)?,
            local_ref: row.get(4)?,
            state: TerminalState::parse(&state).unwrap_or(TerminalState::Closed),
            cols: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
            rows: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
            term: row.get(8)?,
            process_label: row.get(9)?,
            created_at: parse_rfc3339(&created_at).unwrap_or_else(now),
            last_activity_at: parse_rfc3339(&last_activity_at).unwrap_or_else(now),
            closed_at: closed_at.as_deref().and_then(parse_rfc3339),
            close_reason: row.get(13)?,
            durable_offset: row.get::<_, i64>(14)?.max(0) as u64,
            earliest_offset: row.get::<_, i64>(15)?.max(0) as u64,
            accepts_input: row.get::<_, i64>(16)? != 0,
        })
    }
}

pub fn find_open_terminal_by_local_ref(
    conn: &Connection,
    device_id: Uuid,
    local_ref: &str,
) -> ApiResult<Option<TerminalRow>> {
    let sql = format!(
        "SELECT {TERMINAL_COLUMNS} FROM terminals
         WHERE device_id = ?1 AND local_ref = ?2 AND state = 'open'"
    );
    Ok(conn
        .query_row(
            &sql,
            params![device_id.to_string(), local_ref],
            TerminalRow::from_row,
        )
        .optional()?)
}

pub fn get_terminal(conn: &Connection, terminal_id: Uuid) -> ApiResult<Option<TerminalRow>> {
    let sql = format!("SELECT {TERMINAL_COLUMNS} FROM terminals WHERE terminal_id = ?1");
    Ok(conn
        .query_row(
            &sql,
            params![terminal_id.to_string()],
            TerminalRow::from_row,
        )
        .optional()?)
}

pub fn count_open_terminals_for_device(conn: &Connection, device_id: Uuid) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM terminals WHERE device_id = ?1 AND state = 'open'",
        params![device_id.to_string()],
        |row| row.get(0),
    )?)
}

#[allow(clippy::too_many_arguments)]
pub fn insert_terminal(
    txn: &Transaction<'_>,
    device_id: Uuid,
    identity_id: &str,
    local_ref: &str,
    label: &str,
    cols: Option<u32>,
    rows: Option<u32>,
    term: Option<&str>,
    process_label: Option<&str>,
    accepts_input: bool,
    origin: &str,
    requested_by_principal: Option<&str>,
) -> ApiResult<TerminalRow> {
    let terminal_id = Uuid::new_v4();
    let created_at = now();
    txn.execute(
        "INSERT INTO terminals
            (terminal_id, device_id, identity_id, label, local_ref, state, cols, rows, term,
             process_label, accepts_input, created_at, last_activity_at, durable_offset,
             earliest_offset, origin, requested_by_principal)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9, ?11, ?10, ?10, 0, 0, ?12, ?13)",
        params![
            terminal_id.to_string(),
            device_id.to_string(),
            identity_id,
            label,
            local_ref,
            cols.map(|v| v as i64),
            rows.map(|v| v as i64),
            term,
            process_label,
            to_rfc3339(created_at),
            accepts_input as i64,
            origin,
            requested_by_principal,
        ],
    )?;
    Ok(TerminalRow {
        terminal_id,
        device_id,
        identity_id: identity_id.to_string(),
        label: label.to_string(),
        local_ref: local_ref.to_string(),
        state: TerminalState::Open,
        cols,
        rows,
        term: term.map(str::to_string),
        process_label: process_label.map(str::to_string),
        accepts_input,
        created_at,
        last_activity_at: created_at,
        closed_at: None,
        close_reason: None,
        durable_offset: 0,
        earliest_offset: 0,
    })
}

pub fn update_terminal_metadata(
    conn: &Connection,
    terminal_id: Uuid,
    label: &str,
    cols: Option<u32>,
    rows: Option<u32>,
    term: Option<&str>,
) -> ApiResult<()> {
    conn.execute(
        "UPDATE terminals
            SET label = ?2, cols = ?3, rows = ?4, term = COALESCE(?5, term), last_activity_at = ?6
          WHERE terminal_id = ?1",
        params![
            terminal_id.to_string(),
            label,
            cols.map(|v| v as i64),
            rows.map(|v| v as i64),
            term,
            to_rfc3339(now())
        ],
    )?;
    Ok(())
}

pub fn update_terminal_size(
    conn: &Connection,
    terminal_id: Uuid,
    cols: u32,
    rows: u32,
) -> ApiResult<()> {
    conn.execute(
        "UPDATE terminals SET cols = ?2, rows = ?3, last_activity_at = ?4 WHERE terminal_id = ?1",
        params![
            terminal_id.to_string(),
            cols as i64,
            rows as i64,
            to_rfc3339(now())
        ],
    )?;
    Ok(())
}

pub struct TerminalFilters {
    pub device_id: Option<Uuid>,
    pub state: Option<TerminalState>,
}

pub fn list_terminals(
    conn: &Connection,
    identity_id: &str,
    filters: &TerminalFilters,
    cursor: Option<&Cursor>,
    limit: i64,
) -> ApiResult<Vec<TerminalRow>> {
    let mut sql = format!("SELECT {TERMINAL_COLUMNS} FROM terminals WHERE identity_id = ?1");
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(identity_id.to_string())];

    if let Some(device_id) = filters.device_id {
        binds.push(Box::new(device_id.to_string()));
        sql.push_str(&format!(" AND device_id = ?{}", binds.len()));
    }
    if let Some(state) = filters.state {
        binds.push(Box::new(state.as_str().to_string()));
        sql.push_str(&format!(" AND state = ?{}", binds.len()));
    }
    if let Some(cursor) = cursor {
        binds.push(Box::new(cursor.timestamp.clone()));
        let ts_index = binds.len();
        binds.push(Box::new(cursor.id.clone()));
        let id_index = binds.len();
        sql.push_str(&format!(
            " AND (created_at > ?{ts_index} OR (created_at = ?{ts_index} AND terminal_id > ?{id_index}))"
        ));
    }
    binds.push(Box::new(limit));
    sql.push_str(&format!(
        " ORDER BY created_at, terminal_id LIMIT ?{}",
        binds.len()
    ));

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), TerminalRow::from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_open_terminals(conn: &Connection) -> ApiResult<Vec<TerminalRow>> {
    let sql = format!("SELECT {TERMINAL_COLUMNS} FROM terminals WHERE state = 'open'");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], TerminalRow::from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_open_terminals_for_device(
    conn: &Connection,
    device_id: Uuid,
) -> ApiResult<Vec<TerminalRow>> {
    let sql =
        format!("SELECT {TERMINAL_COLUMNS} FROM terminals WHERE device_id = ?1 AND state = 'open'");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![device_id.to_string()], TerminalRow::from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Contiguous retained payload for a terminal, used to rebuild the in-memory ring
/// after a restart.
pub fn load_terminal_output(
    conn: &Connection,
    terminal_id: Uuid,
    earliest_offset: u64,
) -> ApiResult<Vec<u8>> {
    let mut stmt = conn.prepare(
        "SELECT start_offset, end_offset, bytes FROM terminal_output
          WHERE terminal_id = ?1 AND end_offset > ?2
          ORDER BY start_offset",
    )?;
    let rows = stmt.query_map(
        params![terminal_id.to_string(), earliest_offset as i64],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;

    let mut out: Vec<u8> = Vec::new();
    let mut expected = earliest_offset;
    for row in rows {
        let (start, _end, bytes) = row?;
        let start = start.max(0) as u64;
        // Skip any prefix below the retained window, then require contiguity so a
        // corrupt or partially trimmed range is reported instead of silently
        // producing a hole in the replay stream.
        let skip = expected.saturating_sub(start) as usize;
        if start > expected {
            return Err(ApiError::internal(format!(
                "terminal {terminal_id} has a gap in stored output at offset {expected}"
            )));
        }
        if skip >= bytes.len() {
            continue;
        }
        out.extend_from_slice(&bytes[skip..]);
        expected = start + bytes.len() as u64;
    }
    Ok(out)
}

/// One terminal's contribution to a checkpoint transaction.
pub struct TerminalCheckpoint {
    pub terminal_id: Uuid,
    /// Offset of the first byte in `chunk`.
    pub chunk_start: u64,
    pub chunk: Vec<u8>,
    pub earliest_offset: u64,
    /// Offset immediately after the last byte in `chunk`.
    pub durable_offset: u64,
    pub last_activity: DateTime<Utc>,
    /// When set, the terminal is marked closed in the same transaction, so a
    /// subscriber can never see `terminal.closed` before the final bytes commit.
    pub close_reason: Option<String>,
}

/// Commit a batch of checkpoints.
///
/// One transaction covers many frames and many terminals (spec §7.2). Payload is
/// appended as a new chunk rather than rewriting the retained suffix, and eviction
/// is applied as coalesced range deletion plus at most one straddling-chunk rewrite.
pub fn commit_checkpoints(txn: &Transaction<'_>, batch: &[TerminalCheckpoint]) -> ApiResult<u64> {
    let mut rows_written = 0u64;

    for checkpoint in batch {
        let terminal_id = checkpoint.terminal_id.to_string();

        if !checkpoint.chunk.is_empty() {
            txn.execute(
                "INSERT INTO terminal_output (terminal_id, start_offset, end_offset, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    terminal_id,
                    checkpoint.chunk_start as i64,
                    checkpoint.durable_offset as i64,
                    checkpoint.chunk
                ],
            )?;
            rows_written += 1;
        }

        let earliest = checkpoint.earliest_offset as i64;

        // Whole chunks below the retained window: one range delete.
        rows_written += txn.execute(
            "DELETE FROM terminal_output WHERE terminal_id = ?1 AND end_offset <= ?2",
            params![terminal_id, earliest],
        )? as u64;

        // At most one chunk straddles the boundary; trim its evicted prefix.
        let straddling: Option<(i64, i64, Vec<u8>)> = txn
            .query_row(
                "SELECT start_offset, end_offset, bytes FROM terminal_output
                  WHERE terminal_id = ?1 AND start_offset < ?2 AND end_offset > ?2",
                params![terminal_id, earliest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        if let Some((start, end, bytes)) = straddling {
            let skip = (earliest - start).max(0) as usize;
            if skip < bytes.len() {
                txn.execute(
                    "DELETE FROM terminal_output WHERE terminal_id = ?1 AND start_offset = ?2",
                    params![terminal_id, start],
                )?;
                txn.execute(
                    "INSERT INTO terminal_output (terminal_id, start_offset, end_offset, bytes)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![terminal_id, earliest, end, &bytes[skip..]],
                )?;
                rows_written += 2;
            }
        }

        // Publishing the retained range and durable_offset in the same statement as
        // the payload keeps recovery from ever mixing one checkpoint's payload with
        // another's offsets (spec §7.4).
        match &checkpoint.close_reason {
            Some(reason) => {
                txn.execute(
                    "UPDATE terminals
                        SET durable_offset = ?2, earliest_offset = ?3, last_activity_at = ?4,
                            state = 'closed', closed_at = ?5, close_reason = ?6
                      WHERE terminal_id = ?1",
                    params![
                        terminal_id,
                        checkpoint.durable_offset as i64,
                        earliest,
                        to_rfc3339(checkpoint.last_activity),
                        to_rfc3339(now()),
                        reason
                    ],
                )?;
            }
            None => {
                txn.execute(
                    "UPDATE terminals
                        SET durable_offset = ?2, earliest_offset = ?3, last_activity_at = ?4
                      WHERE terminal_id = ?1",
                    params![
                        terminal_id,
                        checkpoint.durable_offset as i64,
                        earliest,
                        to_rfc3339(checkpoint.last_activity)
                    ],
                )?;
            }
        }
        rows_written += 1;
    }

    Ok(rows_written)
}

pub fn close_terminal_without_output(
    conn: &Connection,
    terminal_id: Uuid,
    reason: &str,
) -> ApiResult<()> {
    conn.execute(
        "UPDATE terminals SET state = 'closed', closed_at = ?2, close_reason = ?3
          WHERE terminal_id = ?1 AND state = 'open'",
        params![terminal_id.to_string(), to_rfc3339(now()), reason],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------- retention

pub fn expired_closed_terminals(
    conn: &Connection,
    cutoff: DateTime<Utc>,
    limit: i64,
) -> ApiResult<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT terminal_id FROM terminals
          WHERE state = 'closed' AND closed_at IS NOT NULL AND closed_at < ?1
          ORDER BY closed_at LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![to_rfc3339(cutoff), limit], |row| {
        row.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(Uuid::parse_str(&row?).unwrap_or_default());
    }
    Ok(out)
}

pub fn oldest_closed_terminals(conn: &Connection, limit: i64) -> ApiResult<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT terminal_id FROM terminals
          WHERE state = 'closed' ORDER BY closed_at IS NULL, closed_at LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(Uuid::parse_str(&row?).unwrap_or_default());
    }
    Ok(out)
}

pub fn delete_terminal(txn: &Transaction<'_>, terminal_id: Uuid) -> ApiResult<()> {
    txn.execute(
        "DELETE FROM terminal_output WHERE terminal_id = ?1",
        params![terminal_id.to_string()],
    )?;
    txn.execute(
        "DELETE FROM terminals WHERE terminal_id = ?1",
        params![terminal_id.to_string()],
    )?;
    Ok(())
}

pub fn total_output_bytes(conn: &Connection) -> ApiResult<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM terminal_output",
        [],
        |row| row.get(0),
    )?;
    Ok(total.max(0) as u64)
}

// -------------------------------------------------------------------- challenges

pub struct ChallengeRecord {
    pub challenge_id: String,
    pub operation: Operation,
    pub key: PublicKey,
    pub key_fingerprint: String,
    pub owner_identity_id: Option<String>,
    pub device_key_fingerprint: Option<String>,
    pub challenge: Vec<u8>,
    pub expires_at: DateTime<Utc>,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_challenge(
    conn: &Connection,
    challenge_id: &str,
    operation: Operation,
    key: &PublicKey,
    owner_identity_id: Option<&str>,
    device_key_fingerprint: Option<&str>,
    challenge: &[u8],
    expires_at: DateTime<Utc>,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO challenges
            (challenge_id, operation, algorithm, public_key, key_fingerprint,
             owner_identity_id, device_key_fingerprint, challenge, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            challenge_id,
            operation.as_str(),
            key.algorithm,
            key.bytes,
            key.fingerprint(),
            owner_identity_id,
            device_key_fingerprint,
            challenge,
            to_rfc3339(now()),
            to_rfc3339(expires_at)
        ],
    )?;
    Ok(())
}

/// The outcome of trying to claim a challenge for verification.
pub enum ChallengeClaim {
    Claimed(ChallengeRecord),
    AlreadyConsumed,
    Expired,
    Unknown,
}

/// Atomically mark a challenge consumed and return it.
///
/// Consumption happens before signature verification, so a challenge is invalidated
/// by its first verification attempt whether that attempt succeeds or fails
/// (spec §4.2). This commits immediately: it is a security-critical mutation and
/// must not wait for the output flush interval (spec §7.2).
#[allow(clippy::type_complexity)]
pub fn claim_challenge(txn: &Transaction<'_>, challenge_id: &str) -> ApiResult<ChallengeClaim> {
    let row: Option<(
        String,
        String,
        Vec<u8>,
        String,
        Option<String>,
        Option<String>,
        Vec<u8>,
        String,
        Option<String>,
    )> = txn
        .query_row(
            "SELECT operation, algorithm, public_key, key_fingerprint, owner_identity_id,
                    device_key_fingerprint, challenge, expires_at, consumed_at
               FROM challenges WHERE challenge_id = ?1",
            params![challenge_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?;

    let Some((
        operation,
        algorithm,
        public_key,
        key_fingerprint,
        owner_identity_id,
        device_key_fingerprint,
        challenge,
        expires_at,
        consumed_at,
    )) = row
    else {
        return Ok(ChallengeClaim::Unknown);
    };

    if consumed_at.is_some() {
        return Ok(ChallengeClaim::AlreadyConsumed);
    }

    txn.execute(
        "UPDATE challenges SET consumed_at = ?2 WHERE challenge_id = ?1",
        params![challenge_id, to_rfc3339(now())],
    )?;

    let expires_at = parse_rfc3339(&expires_at).unwrap_or_else(now);
    if expires_at < now() {
        return Ok(ChallengeClaim::Expired);
    }

    let Some(operation) = Operation::parse(&operation) else {
        return Ok(ChallengeClaim::Unknown);
    };

    Ok(ChallengeClaim::Claimed(ChallengeRecord {
        challenge_id: challenge_id.to_string(),
        operation,
        key: PublicKey::from_stored(&algorithm, public_key),
        key_fingerprint,
        owner_identity_id,
        device_key_fingerprint,
        challenge,
        expires_at,
    }))
}

pub fn delete_expired_challenges(conn: &Connection) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM challenges WHERE expires_at < ?1",
        params![to_rfc3339(now() - chrono::Duration::hours(1))],
    )?)
}

// ------------------------------------------------------------------ ws tickets

pub struct TicketRecord {
    pub principal_kind: PrincipalKind,
    pub principal_id: String,
    pub identity_id: String,
    pub scopes: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_ticket(
    conn: &Connection,
    ticket_hash: &str,
    principal_kind: PrincipalKind,
    principal_id: &str,
    identity_id: &str,
    path: &str,
    scopes: &[String],
    expires_at: DateTime<Utc>,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO websocket_tickets
            (ticket_hash, principal_kind, principal_id, identity_id, path, scopes, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ticket_hash,
            principal_kind.as_str(),
            principal_id,
            identity_id,
            path,
            scopes.join(" "),
            to_rfc3339(now()),
            to_rfc3339(expires_at)
        ],
    )?;
    Ok(())
}

/// Consume a ticket for exactly one path.
///
/// Any attempt consumes it, matching or not, so a ticket cannot be probed against
/// several paths (spec §5.1).
#[allow(clippy::type_complexity)]
pub fn consume_ticket(
    txn: &Transaction<'_>,
    ticket_hash: &str,
    path: &str,
) -> ApiResult<Option<TicketRecord>> {
    let row: Option<(String, String, String, String, String, String, Option<String>)> = txn
        .query_row(
            "SELECT principal_kind, principal_id, identity_id, path, scopes, expires_at, consumed_at
               FROM websocket_tickets WHERE ticket_hash = ?1",
            params![ticket_hash],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;

    let Some((kind, principal_id, identity_id, stored_path, scopes, expires_at, consumed_at)) = row
    else {
        return Ok(None);
    };

    if consumed_at.is_some() {
        return Ok(None);
    }
    txn.execute(
        "UPDATE websocket_tickets SET consumed_at = ?2 WHERE ticket_hash = ?1",
        params![ticket_hash, to_rfc3339(now())],
    )?;

    if stored_path != path {
        return Ok(None);
    }
    if parse_rfc3339(&expires_at)
        .map(|t| t < now())
        .unwrap_or(true)
    {
        return Ok(None);
    }

    let principal_kind = match kind.as_str() {
        "device" => PrincipalKind::Device,
        _ => PrincipalKind::Identity,
    };
    Ok(Some(TicketRecord {
        principal_kind,
        principal_id,
        identity_id,
        scopes: scopes.split_whitespace().map(str::to_string).collect(),
    }))
}

pub fn delete_expired_tickets(conn: &Connection) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM websocket_tickets WHERE expires_at < ?1",
        params![to_rfc3339(now() - chrono::Duration::hours(1))],
    )?)
}

// ---------------------------------------------------------------- signing keys

pub struct StoredSigningKey {
    pub kid: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub active: bool,
    pub not_after: Option<DateTime<Utc>>,
}

pub fn load_signing_keys(conn: &Connection) -> ApiResult<Vec<StoredSigningKey>> {
    let mut stmt = conn.prepare(
        "SELECT kid, nonce, ciphertext, created_at, active, not_after FROM signing_keys
          ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let created_at: String = row.get(3)?;
        let not_after: Option<String> = row.get(5)?;
        Ok(StoredSigningKey {
            kid: row.get(0)?,
            nonce: row.get(1)?,
            ciphertext: row.get(2)?,
            created_at: parse_rfc3339(&created_at).unwrap_or_else(now),
            active: row.get::<_, i64>(4)? != 0,
            not_after: not_after.as_deref().and_then(parse_rfc3339),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn insert_signing_key(
    txn: &Transaction<'_>,
    kid: &str,
    nonce: &[u8],
    ciphertext: &[u8],
    overlap_seconds: i64,
) -> ApiResult<()> {
    // The outgoing key keeps verifying for the overlap window so tokens minted a
    // moment before rotation stay valid (spec §8.1).
    txn.execute(
        "UPDATE signing_keys SET active = 0, not_after = ?1 WHERE active = 1",
        params![to_rfc3339(
            now() + chrono::Duration::seconds(overlap_seconds)
        )],
    )?;
    txn.execute(
        "INSERT INTO signing_keys (kid, nonce, ciphertext, created_at, active, not_after)
         VALUES (?1, ?2, ?3, ?4, 1, NULL)",
        params![kid, nonce, ciphertext, to_rfc3339(now())],
    )?;
    Ok(())
}

pub fn prune_signing_keys(conn: &Connection) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM signing_keys WHERE active = 0 AND not_after IS NOT NULL AND not_after < ?1",
        params![to_rfc3339(now())],
    )?)
}

// --------------------------------------------------------------- idempotency

pub struct IdempotentResponse {
    pub request_hash: String,
    pub status: u16,
    pub body: String,
}

pub fn get_idempotent(
    conn: &Connection,
    key_hash: &str,
    principal_id: &str,
) -> ApiResult<Option<IdempotentResponse>> {
    let row: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT request_hash, status, response_body FROM idempotency
              WHERE key_hash = ?1 AND principal_id = ?2",
            params![key_hash, principal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(row.map(|(request_hash, status, body)| IdempotentResponse {
        request_hash,
        status: status as u16,
        body,
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn put_idempotent(
    conn: &Connection,
    key_hash: &str,
    principal_id: &str,
    method: &str,
    path: &str,
    request_hash: &str,
    status: u16,
    body: &str,
) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO idempotency
            (key_hash, principal_id, method, path, request_hash, status, response_body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (key_hash) DO NOTHING",
        params![
            key_hash,
            principal_id,
            method,
            path,
            request_hash,
            status as i64,
            body,
            to_rfc3339(now())
        ],
    )?;
    Ok(())
}

pub fn delete_expired_idempotency(conn: &Connection, retention_seconds: i64) -> ApiResult<usize> {
    let cutoff = now() - chrono::Duration::seconds(retention_seconds);
    Ok(conn.execute(
        "DELETE FROM idempotency WHERE created_at < ?1",
        params![to_rfc3339(cutoff)],
    )?)
}

// ----------------------------------------------------- registration accounting

pub fn record_registration(conn: &Connection, source: &str) -> ApiResult<()> {
    conn.execute(
        "INSERT INTO registration_events (source, at) VALUES (?1, ?2)",
        params![source, to_rfc3339(now())],
    )?;
    Ok(())
}

pub fn count_registrations_since(
    conn: &Connection,
    source: &str,
    since: DateTime<Utc>,
) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM registration_events WHERE source = ?1 AND at >= ?2",
        params![source, to_rfc3339(since)],
        |row| row.get(0),
    )?)
}

pub fn delete_old_registration_events(conn: &Connection) -> ApiResult<usize> {
    Ok(conn.execute(
        "DELETE FROM registration_events WHERE at < ?1",
        params![to_rfc3339(now() - chrono::Duration::days(2))],
    )?)
}

// -------------------------------------------------------------------- cursors

/// Opaque list cursor. Callers see only base64url text (spec §5.2).
pub struct Cursor {
    pub timestamp: String,
    pub id: String,
}

impl Cursor {
    pub fn encode(timestamp: &str, id: &str) -> String {
        b64_encode(format!("{timestamp}|{id}").as_bytes())
    }

    pub fn decode(raw: &str) -> ApiResult<Self> {
        let bytes = b64_decode(raw).ok_or_else(|| ApiError::invalid("malformed cursor"))?;
        let text = String::from_utf8(bytes).map_err(|_| ApiError::invalid("malformed cursor"))?;
        let (timestamp, id) = text
            .split_once('|')
            .ok_or_else(|| ApiError::invalid("malformed cursor"))?;
        Ok(Self {
            timestamp: timestamp.to_string(),
            id: id.to_string(),
        })
    }
}
