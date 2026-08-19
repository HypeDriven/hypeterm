//! Shared application state and background tasks.

use crate::bootstrap::Bootstrap;
use crate::crypto::{SecretBox, SigningKey, TokenClaims, mint_token, verify_token};
use crate::db::{Db, in_txn, repo};
use crate::error::{ApiError, ApiResult};
use crate::metrics;
use crate::relay::registry::Registry;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use crate::settings::store::SettingsStore;
use crate::util::{b64_encode, new_ulid, now, random_bytes};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Token signing keys, rotated with an overlap window so tokens minted just before
/// rotation keep verifying (spec §8.1).
pub struct TokenService {
    db: Db,
    secret_box: Arc<SecretBox>,
    keys: RwLock<Vec<KeyEntry>>,
}

#[derive(Clone)]
struct KeyEntry {
    key: SigningKey,
    created_at: chrono::DateTime<chrono::Utc>,
    active: bool,
}

impl TokenService {
    pub fn load(db: Db, secret_box: Arc<SecretBox>, snapshot: &Snapshot) -> ApiResult<Self> {
        let service = Self {
            db,
            secret_box,
            keys: RwLock::new(Vec::new()),
        };
        service.refresh(snapshot)?;
        Ok(service)
    }

    /// Read keys from durable state, minting the first one if the database is new.
    pub fn refresh(&self, snapshot: &Snapshot) -> ApiResult<()> {
        let mut conn = self.db.conn()?;
        let stored = repo::load_signing_keys(&conn)?;

        let mut entries: Vec<KeyEntry> = Vec::new();
        for key in stored {
            // Expired overlap windows stop verifying.
            if !key.active
                && let Some(not_after) = key.not_after
                && not_after < now()
            {
                continue;
            }
            match self.secret_box.open(&key.nonce, &key.ciphertext) {
                Some(secret) => entries.push(KeyEntry {
                    key: SigningKey {
                        kid: key.kid,
                        secret,
                    },
                    created_at: key.created_at,
                    active: key.active,
                }),
                None => {
                    // Wrong bootstrap key material: fail loudly rather than silently
                    // invalidating every outstanding token.
                    return Err(ApiError::internal(
                        "a stored token signing key could not be decrypted with the supplied bootstrap key material",
                    ));
                }
            }
        }

        if !entries.iter().any(|e| e.active) {
            let overlap = snapshot.int(keys::AUTH_SIGNING_KEY_OVERLAP_SECONDS);
            let kid = new_ulid();
            let secret = random_bytes(32);
            let (nonce, ciphertext) = self.secret_box.seal(&secret);
            in_txn(&mut conn, |txn| {
                repo::insert_signing_key(txn, &kid, &nonce, &ciphertext, overlap)
            })?;
            for entry in entries.iter_mut() {
                entry.active = false;
            }
            entries.insert(
                0,
                KeyEntry {
                    key: SigningKey { kid, secret },
                    created_at: now(),
                    active: true,
                },
            );
            tracing::info!(event = "signing_key_created", "minted a token signing key");
        }

        entries.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then(b.created_at.cmp(&a.created_at))
        });
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = entries;
        Ok(())
    }

    fn active(&self) -> ApiResult<SigningKey> {
        let keys = self.keys.read().unwrap_or_else(|e| e.into_inner());
        keys.iter()
            .find(|e| e.active)
            .map(|e| e.key.clone())
            .ok_or_else(|| ApiError::internal("no active token signing key"))
    }

    fn all(&self) -> Vec<SigningKey> {
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.key.clone())
            .collect()
    }

    pub fn mint(&self, claims: &TokenClaims) -> ApiResult<String> {
        let key = self.active()?;
        let mut claims = claims.clone();
        claims.kid = key.kid.clone();
        Ok(mint_token(&key, &claims))
    }

    pub fn verify(&self, token: &str, snapshot: &Snapshot) -> ApiResult<TokenClaims> {
        let origin = snapshot.string(keys::SERVER_PUBLIC_ORIGIN);
        verify_token(
            token,
            &self.all(),
            &origin,
            &origin,
            now().timestamp(),
            snapshot.int(keys::AUTH_MAX_CLOCK_SKEW_SECONDS),
        )
    }

    /// Rotate when the active key is older than the configured rotation age.
    pub fn rotate_if_needed(&self, snapshot: &Snapshot) -> ApiResult<bool> {
        let rotation =
            chrono::Duration::seconds(snapshot.int(keys::AUTH_SIGNING_KEY_ROTATION_SECONDS));
        let due = {
            let keys_guard = self.keys.read().unwrap_or_else(|e| e.into_inner());
            match keys_guard.iter().find(|e| e.active) {
                Some(entry) => now() - entry.created_at > rotation,
                None => true,
            }
        };
        if !due {
            return Ok(false);
        }

        let overlap = snapshot.int(keys::AUTH_SIGNING_KEY_OVERLAP_SECONDS);
        let kid = new_ulid();
        let secret = random_bytes(32);
        let (nonce, ciphertext) = self.secret_box.seal(&secret);
        let mut conn = self.db.conn()?;
        in_txn(&mut conn, |txn| {
            repo::insert_signing_key(txn, &kid, &nonce, &ciphertext, overlap)
        })?;
        drop(conn);
        self.refresh(snapshot)?;
        tracing::info!(
            event = "signing_key_rotated",
            "rotated the token signing key"
        );
        Ok(true)
    }
}

/// In-memory token buckets for request rate limiting (spec §4.2, §10).
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consume one token. Returns false when the caller is over its allowance.
    pub fn check(&self, scope: &str, key: &str, per_minute: i64) -> bool {
        self.check_window(scope, key, per_minute, 60)
    }

    /// Same, over an arbitrary window. Terminal-open requests are counted per hour
    /// rather than per minute: a handful an hour is generous for a person opening tabs
    /// and still bounds what a stolen credential can start.
    pub fn check_window(&self, scope: &str, key: &str, capacity: i64, window_seconds: i64) -> bool {
        if capacity <= 0 || window_seconds <= 0 {
            return false;
        }
        let capacity = capacity as f64;
        let refill_per_second = capacity / window_seconds as f64;
        let composite = format!("{scope}\u{1}{key}");

        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets.entry(composite).or_insert_with(|| Bucket {
            tokens: capacity,
            last_refill: Instant::now(),
        });

        let elapsed = bucket.last_refill.elapsed().as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
        bucket.last_refill = Instant::now();

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drop buckets that have been idle long enough to have fully refilled.
    pub fn sweep(&self) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        buckets.retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(600));
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AppState {
    pub db: Db,
    pub settings: Arc<SettingsStore>,
    pub registry: Arc<Registry>,
    pub tokens: TokenService,
    pub rate_limiter: RateLimiter,
    pub bootstrap: Bootstrap,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Set once startup recovery has finished.
    pub started: AtomicBool,
}

impl AppState {
    pub fn new(bootstrap: Bootstrap) -> ApiResult<Arc<Self>> {
        let db = Db::open(&bootstrap.db_path)?;
        let settings = SettingsStore::initialize(&db, &bootstrap)?;
        let snapshot = settings.snapshot();
        metrics::SETTINGS_REVISION.set(snapshot.revision);
        metrics::SETTINGS_COMMITTED_REVISION.set(snapshot.revision);

        // Apply storage pragmas from the loaded snapshot.
        db.pragmas.update(
            &snapshot.string(keys::PERSISTENCE_SQLITE_SYNCHRONOUS),
            snapshot.int(keys::PERSISTENCE_SQLITE_BUSY_TIMEOUT_MS),
            snapshot.int(keys::PERSISTENCE_SQLITE_CACHE_SIZE_KIB),
            snapshot.int(keys::PERSISTENCE_SQLITE_WAL_AUTOCHECKPOINT_PAGES),
        );

        let tokens = TokenService::load(db.clone(), settings.secret_box(), &snapshot)?;
        let registry = Registry::new(db.clone(), Arc::clone(&settings));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        Ok(Arc::new(Self {
            db,
            settings,
            registry,
            tokens,
            rate_limiter: RateLimiter::new(),
            bootstrap,
            shutdown_tx,
            shutdown_rx,
            started: AtomicBool::new(false),
        }))
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.settings.snapshot()
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown_rx.borrow()
    }

    /// Start background work: settings propagation, checkpoints, and maintenance.
    pub async fn start_background(self: &Arc<Self>) -> ApiResult<()> {
        self.settings
            .spawn_propagation_watcher(self.shutdown_rx.clone());

        tokio::spawn(crate::relay::flush::run(
            Arc::clone(&self.registry),
            self.shutdown_rx.clone(),
        ));

        tokio::spawn(maintenance(Arc::clone(self), self.shutdown_rx.clone()));

        tokio::spawn(settings_effects(Arc::clone(self)));

        self.registry.recover_open_terminals().await?;
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    /// Readiness: authentication works, durable state reads and writes, relay traffic
    /// is being accepted, and the committed settings revision is loadable (spec §5.4).
    pub async fn readiness(&self) -> Result<(), String> {
        if self.is_shutting_down() {
            return Err("server is shutting down".to_string());
        }
        if !self.started.load(Ordering::Acquire) {
            return Err("startup recovery has not finished".to_string());
        }
        if self.registry.storage_failing() {
            return Err("durable storage is failing".to_string());
        }
        if self.tokens.verify("", &self.snapshot()).is_ok() {
            return Err("token verification is misconfigured".to_string());
        }

        let settings = Arc::clone(&self.settings);
        let db = self.db.clone();
        let probe = tokio::task::spawn_blocking(move || -> ApiResult<()> {
            // Prove the settings revision still loads and validates.
            settings.verify_loadable()?;
            // Prove durable state is readable and writable.
            let mut conn = db.conn()?;
            in_txn(&mut conn, |txn| {
                txn.execute(
                    "INSERT INTO schema_meta (key, value) VALUES ('readiness_probe', ?1)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![crate::util::to_rfc3339(now())],
                )?;
                Ok(())
            })
        })
        .await;

        match probe {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.message),
            Err(e) => Err(format!("readiness probe failed: {e}")),
        }
    }
}

/// React to settings revisions that need more than a snapshot swap: storage pragmas,
/// logging, and the metrics gauge.
async fn settings_effects(state: Arc<AppState>) {
    let mut receiver = state.settings.subscribe();
    loop {
        if receiver.changed().await.is_err() {
            return;
        }
        let snapshot = state.settings.snapshot();
        metrics::SETTINGS_REVISION.set(snapshot.revision);

        let changed = state.db.pragmas.update(
            &snapshot.string(keys::PERSISTENCE_SQLITE_SYNCHRONOUS),
            snapshot.int(keys::PERSISTENCE_SQLITE_BUSY_TIMEOUT_MS),
            snapshot.int(keys::PERSISTENCE_SQLITE_CACHE_SIZE_KIB),
            snapshot.int(keys::PERSISTENCE_SQLITE_WAL_AUTOCHECKPOINT_PAGES),
        );
        if changed {
            tracing::info!(
                event = "storage_reconfigured",
                revision = snapshot.revision,
                "storage pragmas will be re-applied to pooled connections"
            );
        }

        crate::observability::apply_log_settings(&snapshot);

        // A replay-capacity reduction must bound memory promptly, not at the next
        // append, so it is applied to resident buffers here.
        let capacity = snapshot.replay_capacity();
        for handle in state.registry.resident_handles() {
            handle.shrink_to_capacity(capacity);
        }
    }
}

async fn maintenance(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut settings_rx = state.settings.subscribe();
    loop {
        let snapshot = state.snapshot();
        let interval = snapshot.duration_secs(keys::PERSISTENCE_RETENTION_SWEEP_INTERVAL_SECONDS);

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            // Waking on a settings revision is what makes the interval itself
            // runtime tunable: without this, a new interval would not take effect
            // until the previous, possibly much longer, sleep elapsed.
            changed = settings_rx.changed() => {
                if changed.is_err() { return; }
                continue;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }

        if let Err(e) = run_maintenance(&state).await {
            tracing::warn!(event = "maintenance_failed", error = %e, "maintenance sweep failed");
        }
    }
}

async fn run_maintenance(state: &Arc<AppState>) -> ApiResult<()> {
    let snapshot = state.snapshot();

    state.rate_limiter.sweep();
    state.registry.evict_retired();
    // Twice the ceiling of the open-request timeout, so an entry can only be swept once
    // every caller that could still be waiting on it has certainly given up.
    state
        .registry
        .sweep_pending_opens(std::time::Duration::from_secs(60));

    let retention = snapshot.int(keys::TERMINAL_CLOSED_RETENTION_SECONDS);
    let quota = snapshot.u64(keys::PERSISTENCE_STORAGE_QUOTA_BYTES);
    let idempotency_retention = snapshot.int(keys::IDEMPOTENCY_RETENTION_SECONDS);

    let db = state.db.clone();
    let deleted = db
        .call(move |conn| {
            repo::delete_expired_challenges(conn)?;
            repo::delete_expired_tickets(conn)?;
            repo::delete_expired_idempotency(conn, idempotency_retention)?;
            repo::delete_old_registration_events(conn)?;
            repo::prune_signing_keys(conn)?;

            // Expired closed terminals go first (spec §7.3).
            let cutoff = now() - chrono::Duration::seconds(retention);
            let expired = repo::expired_closed_terminals(conn, cutoff, 256)?;
            let mut deleted = Vec::new();
            for terminal_id in expired {
                in_txn(conn, |txn| repo::delete_terminal(txn, terminal_id))?;
                deleted.push(terminal_id);
                metrics::RETENTION_TERMINALS_DELETED.inc();
            }

            // Then, if still over quota, the oldest closed terminals. An open
            // terminal is never trimmed below its configured window to satisfy a
            // global quota (spec §7.3).
            let mut used = repo::total_output_bytes(conn)?;
            if used > quota {
                for terminal_id in repo::oldest_closed_terminals(conn, 256)? {
                    if used <= quota {
                        break;
                    }
                    in_txn(conn, |txn| repo::delete_terminal(txn, terminal_id))?;
                    deleted.push(terminal_id);
                    metrics::QUOTA_TERMINALS_DELETED.inc();
                    used = repo::total_output_bytes(conn)?;
                }
            }

            Ok((deleted, used))
        })
        .await?;

    let (deleted, used) = deleted;
    for terminal_id in deleted {
        state.registry.forget(terminal_id);
    }

    metrics::STORAGE_BYTES.set(state.db.storage_bytes() as i64);

    // Storage that cannot be brought under quota must fail readiness rather than
    // silently under-retaining open terminals (spec §7.3).
    if used > quota {
        tracing::error!(
            event = "storage_quota_exceeded",
            used,
            quota,
            "storage quota cannot be satisfied by deleting closed terminals"
        );
        state.registry.set_storage_failing(true);
    }

    if let Err(e) = state.tokens.rotate_if_needed(&snapshot) {
        tracing::warn!(event = "signing_key_rotation_failed", error = %e);
    }

    // Track the committed revision so propagation lag is observable.
    let db = state.db.clone();
    if let Ok(revision) = db
        .call(|conn| {
            Ok(conn.query_row(
                "SELECT revision FROM settings_state WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
    {
        metrics::SETTINGS_COMMITTED_REVISION.set(revision);
    }

    Ok(())
}

/// Build a token for an authenticated principal.
pub fn build_claims(
    snapshot: &Snapshot,
    principal: crate::crypto::PrincipalKind,
    subject: &str,
    identity_id: &str,
    scopes: Vec<String>,
) -> TokenClaims {
    let issued = now();
    let ttl = snapshot.int(keys::AUTH_ACCESS_TOKEN_TTL_SECONDS);
    let origin = snapshot.string(keys::SERVER_PUBLIC_ORIGIN);
    TokenClaims {
        jti: b64_encode(&random_bytes(16)),
        sub: subject.to_string(),
        principal,
        identity_id: identity_id.to_string(),
        iss: origin.clone(),
        aud: origin,
        iat: issued.timestamp(),
        exp: (issued + chrono::Duration::seconds(ttl)).timestamp(),
        scopes,
        kid: String::new(),
    }
}
