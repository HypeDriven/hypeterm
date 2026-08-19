//! Durable state: connection pool, schema, and storage pragmas.
//!
//! Every durable object the specification names — identities, devices, terminal
//! metadata, committed offsets, challenges, revocations, settings and replay
//! checkpoints — lives here, never on the container's writable layer (spec §8).

pub mod repo;

use crate::error::{ApiError, ApiResult};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

pub const SCHEMA: &str = include_str!("schema.sql");

/// Per-connection SQLite tuning, driven entirely by database settings.
///
/// `generation` is bumped whenever any pragma changes; a pooled connection
/// re-applies pragmas on checkout when its generation is stale, so a settings
/// change takes effect without restarting the process.
#[derive(Debug)]
pub struct StoragePragmas {
    pub generation: AtomicU64,
    pub synchronous: std::sync::Mutex<String>,
    pub busy_timeout_ms: AtomicI64,
    pub cache_size_kib: AtomicI64,
    pub wal_autocheckpoint_pages: AtomicI64,
}

impl Default for StoragePragmas {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(1),
            synchronous: std::sync::Mutex::new("normal".to_string()),
            busy_timeout_ms: AtomicI64::new(5000),
            cache_size_kib: AtomicI64::new(8192),
            wal_autocheckpoint_pages: AtomicI64::new(4000),
        }
    }
}

impl StoragePragmas {
    /// Update from a settings snapshot; returns true when something changed.
    pub fn update(
        &self,
        synchronous: &str,
        busy_timeout_ms: i64,
        cache_size_kib: i64,
        wal_autocheckpoint_pages: i64,
    ) -> bool {
        let mut changed = false;
        {
            let mut cur = self.synchronous.lock().expect("pragma lock");
            if *cur != synchronous {
                *cur = synchronous.to_string();
                changed = true;
            }
        }
        for (cell, value) in [
            (&self.busy_timeout_ms, busy_timeout_ms),
            (&self.cache_size_kib, cache_size_kib),
            (&self.wal_autocheckpoint_pages, wal_autocheckpoint_pages),
        ] {
            if cell.swap(value, Ordering::Relaxed) != value {
                changed = true;
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    fn apply(&self, conn: &Connection) -> rusqlite::Result<()> {
        let synchronous = self.synchronous.lock().expect("pragma lock").clone();
        conn.pragma_update(None, "synchronous", &synchronous)?;
        conn.busy_timeout(Duration::from_millis(
            self.busy_timeout_ms.load(Ordering::Relaxed).max(0) as u64,
        ))?;
        // Negative cache_size is interpreted by SQLite as a KiB budget.
        conn.pragma_update(
            None,
            "cache_size",
            -self.cache_size_kib.load(Ordering::Relaxed),
        )?;
        conn.pragma_update(
            None,
            "wal_autocheckpoint",
            self.wal_autocheckpoint_pages.load(Ordering::Relaxed),
        )?;
        Ok(())
    }
}

/// A pooled connection that remembers which pragma generation it has applied.
pub struct RelayConn {
    conn: Connection,
    generation: u64,
}

impl std::ops::Deref for RelayConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl std::ops::DerefMut for RelayConn {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

pub struct SqliteManager {
    path: PathBuf,
    pragmas: Arc<StoragePragmas>,
}

impl r2d2::ManageConnection for SqliteManager {
    type Connection = RelayConn;
    type Error = rusqlite::Error;

    fn connect(&self) -> Result<RelayConn, rusqlite::Error> {
        let conn = Connection::open(&self.path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        self.pragmas.apply(&conn)?;
        Ok(RelayConn {
            conn,
            generation: self.pragmas.generation.load(Ordering::Relaxed),
        })
    }

    fn is_valid(&self, conn: &mut RelayConn) -> Result<(), rusqlite::Error> {
        let current = self.pragmas.generation.load(Ordering::Relaxed);
        if conn.generation != current {
            self.pragmas.apply(&conn.conn)?;
            conn.generation = current;
        }
        conn.conn.execute_batch("SELECT 1;")
    }

    fn has_broken(&self, _conn: &mut RelayConn) -> bool {
        false
    }
}

pub type Pool = r2d2::Pool<SqliteManager>;
pub type PooledConn = r2d2::PooledConnection<SqliteManager>;

#[derive(Clone)]
pub struct Db {
    pool: Pool,
    pub pragmas: Arc<StoragePragmas>,
    path: PathBuf,
}

impl Db {
    /// Open (creating if needed) the database and apply the schema.
    pub fn open(path: &Path) -> ApiResult<Self> {
        let pragmas = Arc::new(StoragePragmas::default());

        // WAL is a persistent database property; set it once on a dedicated
        // connection before the pool starts handing connections out.
        {
            let conn = Connection::open(path)
                .map_err(|e| ApiError::internal(format!("cannot open database: {e}")))?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(|e| ApiError::internal(format!("cannot enable WAL: {e}")))?;
            conn.busy_timeout(Duration::from_millis(5000))
                .map_err(|e| ApiError::internal(format!("cannot set busy timeout: {e}")))?;
            conn.execute_batch(SCHEMA)
                .map_err(|e| ApiError::internal(format!("cannot apply schema: {e}")))?;
            migrate(&conn)?;
        }

        let manager = SqliteManager {
            path: path.to_path_buf(),
            pragmas: Arc::clone(&pragmas),
        };
        let pool = r2d2::Pool::builder()
            .max_size(8)
            .min_idle(Some(1))
            .test_on_check_out(true)
            .connection_timeout(Duration::from_secs(10))
            .build(manager)
            .map_err(|e| ApiError::internal(format!("cannot build database pool: {e}")))?;

        Ok(Self {
            pool,
            pragmas,
            path: path.to_path_buf(),
        })
    }

    pub fn conn(&self) -> ApiResult<PooledConn> {
        self.pool
            .get()
            .map_err(|e| ApiError::storage_unavailable(format!("database unavailable: {e}")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run a blocking database closure on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut RelayConn) -> ApiResult<T> + Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool
                .get()
                .map_err(|e| ApiError::storage_unavailable(format!("database unavailable: {e}")))?;
            f(&mut conn)
        })
        .await
        .map_err(|e| ApiError::internal(format!("database task failed: {e}")))?
    }

    /// Size on disk, including WAL, for quota accounting.
    pub fn storage_bytes(&self) -> u64 {
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            if let Ok(meta) = std::fs::metadata(PathBuf::from(p)) {
                total += meta.len();
            }
        }
        total
    }
}

/// Bring an existing database up to the current schema.
///
/// `schema.sql` uses `CREATE TABLE IF NOT EXISTS`, which does nothing to a table that
/// already exists, so columns added after a database was first created are applied
/// here. Each step is idempotent and checked against `PRAGMA table_info`.
fn migrate(conn: &Connection) -> ApiResult<()> {
    // (table, column, definition) — added when the column is absent.
    const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
        // Protocol version 2: device roles and the per-terminal input opt-in (spec §3.2, §4.5).
        ("devices", "role", "TEXT NOT NULL DEFAULT 'publisher'"),
        ("terminals", "accepts_input", "INTEGER NOT NULL DEFAULT 0"),
        // Who caused this terminal to exist (spec §4.6). Recorded so that a process
        // started from a phone can be told apart afterwards from one the machine's
        // owner started, and traced to the principal that asked.
        ("terminals", "origin", "TEXT NOT NULL DEFAULT 'publisher'"),
        ("terminals", "requested_by_principal", "TEXT"),
    ];

    for (table, column, definition) in ADDED_COLUMNS {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<_, _>>()?;
        if existing.iter().any(|name| name == column) {
            continue;
        }
        // A CHECK constraint cannot be added by ALTER TABLE; the value is validated in
        // code on the way in, and the constraint applies to newly created databases.
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .map_err(|e| ApiError::internal(format!("cannot add {table}.{column}: {e}")))?;
        tracing::info!(
            event = "schema_migrated",
            table = *table,
            column = *column,
            "added a column to an existing database"
        );
    }
    Ok(())
}

/// Run `f` inside an IMMEDIATE transaction, so write intent is taken up front and
/// two concurrent writers fail fast rather than deadlocking mid-transaction.
pub fn in_txn<T>(
    conn: &mut Connection,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> ApiResult<T>,
) -> ApiResult<T> {
    let txn = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let out = f(&txn)?;
    txn.commit()?;
    Ok(out)
}
