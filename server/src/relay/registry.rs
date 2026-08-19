//! The resident terminal set, publisher ownership, and connection accounting.

use super::messages::ProtocolVersion;
use super::terminal::{Lifecycle, TerminalHandle, Termination};
use crate::db::repo::{self, Device, TerminalRow};
use crate::db::{Db, in_txn};
use crate::error::{ApiError, ApiResult};
use crate::metrics;
use crate::settings::defs::keys;
use crate::settings::store::SettingsStore;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

/// One device's publisher slot. Only one publisher may control a device at a time
/// (spec §6.1).
struct PublisherSlot {
    generation: u64,
    connection_id: String,
    supersede: Arc<Notify>,
    version: ProtocolVersion,
    /// Present only for version 2 publishers, which can receive input frames.
    input_tx: Option<tokio::sync::mpsc::Sender<PublisherDelivery>>,
    /// Whether this connection asserted that its machine allows subscribers to ask it
    /// to open a terminal (spec §4.6 condition 2). Per connection, never inherited: a
    /// reconnect must assert it again, and a superseded connection cannot re-grant it.
    open_requests: bool,
}

/// Something a subscriber is sending toward the publishing device (spec §6.3).
#[derive(Debug, Clone)]
pub enum PublisherDelivery {
    Input {
        terminal_id: Uuid,
        relay_sequence: u64,
        payload: Bytes,
    },
    /// A resize the publisher may honour or ignore; it stays the sole authority over
    /// the terminal's dimensions.
    ResizeRequest {
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
    },
    /// A request to open a terminal, which the publisher may decline (spec §4.6).
    OpenRequestDelivery {
        request_id: String,
        label: Option<String>,
        cols: Option<u32>,
        rows: Option<u32>,
    },
}

/// How a terminal came to exist. Recorded so a process a phone asked for can be told
/// apart afterwards from one the machine's owner started (spec §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOrigin {
    Publisher,
    Request,
}

impl TerminalOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Publisher => "publisher",
            Self::Request => "request",
        }
    }
}

/// What became of a terminal-open request.
#[derive(Debug, Clone)]
pub enum OpenOutcome {
    Opened {
        terminal_id: Uuid,
        deduplicated: bool,
    },
    /// The publisher answered and refused. The reason is from a closed set; the
    /// publisher's free-text detail never travels this far.
    Declined { reason: String },
    /// The publisher accepted but the open itself failed — a terminal limit, storage.
    Failed { code: &'static str, message: String },
    /// The publisher went away before answering.
    Unavailable,
}

/// The result of registering a request: whether this caller owns the round trip or
/// merely joined one already in flight.
pub enum BeginOpen {
    Fresh(tokio::sync::watch::Receiver<Option<OpenOutcome>>),
    Joined(tokio::sync::watch::Receiver<Option<OpenOutcome>>),
}

struct PendingOpen {
    tx: tokio::sync::watch::Sender<Option<OpenOutcome>>,
    principal_id: String,
    created: Instant,
}

/// Why input could not be delivered. Both variants are transient: the subscription
/// stays open and the client may retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDeliveryError {
    /// No publisher is connected for the device, or it negotiated version 1.
    NoPublisher,
    /// The publisher's inbound queue is full.
    Backpressure,
}

pub struct Registry {
    db: Db,
    settings: Arc<SettingsStore>,
    terminals: RwLock<HashMap<Uuid, Arc<TerminalHandle>>>,
    publishers: Mutex<HashMap<Uuid, PublisherSlot>>,
    connections: Mutex<HashMap<String, usize>>,
    next_generation: AtomicU64,
    /// Serialises loading so two subscribers cannot build two handles for one terminal.
    load_lock: tokio::sync::Mutex<()>,
    /// Serialises checkpoints so chunk ranges can never overlap.
    pub flush_lock: tokio::sync::Mutex<()>,
    flush_request: Notify,
    storage_failing: AtomicBool,
    shutting_down: AtomicBool,
    /// Terminal-open requests awaiting a publisher's answer, keyed by device and the
    /// caller's derived request id (spec §4.6). A `watch` rather than a `oneshot` so a
    /// concurrent retry of the same idempotency key *joins* the round trip instead of
    /// starting a second one — which would spawn a second shell.
    pending_opens: Mutex<HashMap<(Uuid, String), PendingOpen>>,
}

/// Held for the lifetime of a publisher connection. On drop, the device's terminals
/// enter the reconnect grace period.
pub struct PublisherLease {
    registry: Arc<Registry>,
    device_id: Uuid,
    generation: u64,
    pub supersede: Arc<Notify>,
    pub connection_id: String,
}

impl Drop for PublisherLease {
    fn drop(&mut self) {
        let registry = Arc::clone(&self.registry);
        let device_id = self.device_id;
        let generation = self.generation;
        tokio::spawn(async move {
            registry.on_publisher_detached(device_id, generation).await;
        });
    }
}

/// Bounds concurrent WebSocket connections per principal (spec §10).
pub struct ConnectionPermit {
    registry: Arc<Registry>,
    principal: String,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let mut counts = self
            .registry
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&self.principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.principal);
            }
        }
    }
}

pub struct OpenRequest {
    pub local_ref: String,
    pub label: String,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub term: Option<String>,
    pub process_label: Option<String>,
    /// The publisher's input opt-in for this terminal (spec §4.5).
    pub accepts_input: bool,
    /// How this terminal came to exist (spec §4.6). Decided by the relay from its own
    /// pending-request table, never from anything the publisher asserts.
    pub origin: TerminalOrigin,
    pub requested_by_principal: Option<String>,
}

/// Aggregate dirty-state view used by the checkpoint loop.
pub struct DirtyStats {
    pub total_dirty_bytes: u64,
    pub oldest_dirty: Option<Duration>,
    pub has_pending_close: bool,
    pub lag_bytes: u64,
}

impl Registry {
    pub fn new(db: Db, settings: Arc<SettingsStore>) -> Arc<Self> {
        Arc::new(Self {
            db,
            settings,
            terminals: RwLock::new(HashMap::new()),
            publishers: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            load_lock: tokio::sync::Mutex::new(()),
            flush_lock: tokio::sync::Mutex::new(()),
            flush_request: Notify::new(),
            storage_failing: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            pending_opens: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn settings(&self) -> &Arc<SettingsStore> {
        &self.settings
    }

    pub fn storage_failing(&self) -> bool {
        self.storage_failing.load(Ordering::Acquire)
    }

    pub fn set_storage_failing(&self, failing: bool) {
        let previous = self.storage_failing.swap(failing, Ordering::AcqRel);
        if previous != failing {
            metrics::STORAGE_UNAVAILABLE.set(if failing { 1 } else { 0 });
            if failing {
                tracing::error!(
                    event = "storage_unavailable",
                    "durable storage is failing; readiness is degraded and publishers will be told"
                );
            } else {
                tracing::info!(
                    event = "storage_recovered",
                    "durable storage is healthy again"
                );
            }
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    // ----------------------------------------------------------------- connections

    pub fn acquire_connection(
        self: &Arc<Self>,
        principal: &str,
        max: usize,
    ) -> Option<ConnectionPermit> {
        let mut counts = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(principal.to_string()).or_insert(0);
        if *count >= max {
            if *count == 0 {
                counts.remove(principal);
            }
            return None;
        }
        *count += 1;
        Some(ConnectionPermit {
            registry: Arc::clone(self),
            principal: principal.to_string(),
        })
    }

    // ------------------------------------------------------------------ publishers

    /// Take over a device's publisher slot, superseding any existing connection.
    pub fn attach_publisher(
        self: &Arc<Self>,
        device_id: Uuid,
        connection_id: &str,
        version: ProtocolVersion,
        input_tx: Option<tokio::sync::mpsc::Sender<PublisherDelivery>>,
    ) -> PublisherLease {
        let generation = self.next_generation.fetch_add(1, Ordering::AcqRel);
        let supersede = Arc::new(Notify::new());

        let previous = {
            let mut publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
            publishers.insert(
                device_id,
                PublisherSlot {
                    generation,
                    connection_id: connection_id.to_string(),
                    supersede: Arc::clone(&supersede),
                    version,
                    input_tx,
                    open_requests: false,
                },
            )
        };

        if let Some(previous) = previous {
            // The connection that was asked is gone. A request is never carried over to
            // its replacement: the replacement has not asserted the capability yet, and
            // may belong to a machine whose owner has since turned it off (spec §4.6).
            self.fail_open_requests_for_device(device_id);
            metrics::PUBLISHERS_SUPERSEDED.inc();
            tracing::info!(
                event = "publisher_superseded",
                device_id = %device_id,
                superseded_connection_id = %previous.connection_id,
                connection_id = %connection_id,
                "a newer publisher connection took over the device"
            );
            previous.supersede.notify_waiters();
        }

        PublisherLease {
            registry: Arc::clone(self),
            device_id,
            generation,
            supersede,
            connection_id: connection_id.to_string(),
        }
    }

    async fn on_publisher_detached(self: Arc<Self>, device_id: Uuid, generation: u64) {
        // Before the reconnect grace period, not after: a terminal-open request is
        // never queued for a device that is not connected, so the caller learns at once
        // rather than waiting out a grace period it knows nothing about (spec §4.6).
        self.fail_open_requests_for_device(device_id);
        {
            let mut publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
            match publishers.get(&device_id) {
                // A newer connection already owns the slot; nothing to do.
                Some(slot) if slot.generation != generation => return,
                Some(_) => {
                    publishers.remove(&device_id);
                }
                None => return,
            }
        }

        if self.is_shutting_down() {
            // A shutdown keeps terminals open so they resume after restart.
            return;
        }

        let grace = self
            .settings
            .snapshot()
            .duration_secs(keys::TERMINAL_PUBLISHER_RECONNECT_GRACE_SECONDS);
        tracing::info!(
            event = "publisher_disconnected",
            device_id = %device_id,
            grace_seconds = grace.as_secs(),
            "publisher disconnected; terminals stay open for the reconnect grace period"
        );

        tokio::time::sleep(grace).await;

        // Reconnected inside the grace period, or shutting down: leave terminals open.
        if self
            .publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&device_id)
            || self.is_shutting_down()
        {
            return;
        }

        if let Err(e) = self
            .close_device_terminals(device_id, "publisher_disconnected")
            .await
        {
            tracing::error!(
                event = "publisher_grace_close_failed",
                device_id = %device_id,
                error = %e,
                "could not close terminals after the publisher grace period"
            );
        }
    }

    /// Close every open terminal of a device, committing the close with any
    /// outstanding output.
    pub async fn close_device_terminals(
        self: &Arc<Self>,
        device_id: Uuid,
        reason: &str,
    ) -> ApiResult<()> {
        let rows = {
            let db = self.db.clone();
            db.call(move |conn| repo::list_open_terminals_for_device(conn, device_id))
                .await?
        };

        for row in rows {
            match self.get_or_load(row.terminal_id).await? {
                Some(handle) => {
                    handle.begin_close(reason);
                }
                None => {
                    let db = self.db.clone();
                    let terminal_id = row.terminal_id;
                    let reason = reason.to_string();
                    db.call(move |conn| {
                        repo::close_terminal_without_output(conn, terminal_id, &reason)
                    })
                    .await?;
                    // Closed without a resident handle, so the checkpoint task will
                    // not account for it.
                    metrics::TERMINALS_OPEN.dec();
                    metrics::TERMINALS_CLOSED_TOTAL.inc();
                }
            }
        }
        self.request_flush();
        Ok(())
    }

    /// Whether a version 2 publisher is currently connected for this device, which is
    /// one of the four conditions input delivery requires (spec §4.5).
    pub fn publisher_accepts_input(&self, device_id: Uuid) -> bool {
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&device_id)
            .map(|slot| slot.version.supports_input() && slot.input_tx.is_some())
            .unwrap_or(false)
    }

    /// Whether any publisher connection is currently live for this device.
    pub fn publisher_connected(&self, device_id: Uuid) -> bool {
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&device_id)
    }

    /// Whether a connected publisher for this device may be asked to open a terminal
    /// (spec §4.6 condition 2 and 4).
    ///
    /// Deliberately not expressed in terms of `publisher_accepts_input`: that answers a
    /// different question about a different capability, and a future change to either
    /// must not silently move the other.
    pub fn publisher_supports_open_request(&self, device_id: Uuid) -> bool {
        self.publishers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&device_id)
            .map(|slot| {
                slot.open_requests && slot.version.supports_input() && slot.input_tx.is_some()
            })
            .unwrap_or(false)
    }

    /// Records a connection's capability assertion.
    ///
    /// Ignored unless `connection_id` is the slot's current one, so a connection that
    /// has already been superseded cannot grant a capability to its replacement.
    pub fn set_publisher_open_requests(&self, device_id: Uuid, connection_id: &str, on: bool) {
        let mut publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = publishers.get_mut(&device_id)
            && slot.connection_id == connection_id
        {
            slot.open_requests = on;
        }
    }

    // ------------------------------------------------ terminal-open requests (§4.6)

    /// Registers a request, or joins one already in flight for the same key.
    ///
    /// Registered *before* anything is sent, so a publisher that answers instantly can
    /// never resolve an entry that does not exist yet. The caller must remove it with
    /// `resolve_open_request` if the send then fails, or the waiter dangles until the
    /// sweep.
    pub fn begin_open_request(
        &self,
        device_id: Uuid,
        request_id: &str,
        principal_id: &str,
        max_per_device: usize,
        max_total: usize,
    ) -> ApiResult<BeginOpen> {
        let mut pending = self.pending_opens.lock().unwrap_or_else(|e| e.into_inner());
        let key = (device_id, request_id.to_string());
        if let Some(existing) = pending.get(&key) {
            return Ok(BeginOpen::Joined(existing.tx.subscribe()));
        }
        // Bounded by count rather than by time: what protects the relay and the target
        // machine is how many requests can be outstanding at once, not how long each
        // one may take.
        if pending.len() >= max_total {
            return Err(ApiError::limit_exceeded(
                "too many terminal-open requests are in flight",
            ));
        }
        let for_device = pending.keys().filter(|(id, _)| *id == device_id).count();
        if for_device >= max_per_device {
            return Err(ApiError::limit_exceeded(
                "too many terminal-open requests are in flight for that device",
            ));
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        pending.insert(
            key,
            PendingOpen {
                tx,
                principal_id: principal_id.to_string(),
                created: Instant::now(),
            },
        );
        metrics::TERMINAL_OPEN_REQUESTS_PENDING.set(pending.len() as i64);
        Ok(BeginOpen::Fresh(rx))
    }

    /// Publishes an outcome. Returns false when nothing was waiting, which is how a
    /// forged or replayed `in_reply_to` is detected.
    ///
    /// The entry deliberately *stays* until the caller that owns it calls
    /// `finish_open_request`. Removing it here would reopen the window this whole
    /// mechanism exists to close: between the answer arriving and the idempotency
    /// record being stored, a concurrent retry of the same key would find neither, ask
    /// again, and start a second shell.
    pub fn resolve_open_request(
        &self,
        device_id: Uuid,
        request_id: &str,
        outcome: OpenOutcome,
    ) -> bool {
        let pending = self.pending_opens.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = pending.get(&(device_id, request_id.to_string())) else {
            return false;
        };
        let _ = entry.tx.send(Some(outcome));
        true
    }

    /// Retires an entry once its owner has finished responding.
    ///
    /// Called after the idempotency record is durable, so from here on a retry of the
    /// same key is answered from storage rather than by asking the machine again.
    pub fn finish_open_request(&self, device_id: Uuid, request_id: &str) {
        let mut pending = self.pending_opens.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&(device_id, request_id.to_string()));
        metrics::TERMINAL_OPEN_REQUESTS_PENDING.set(pending.len() as i64);
    }

    /// The principal that asked for a pending request, for the forensic columns.
    ///
    /// Read from the relay's own record, never from anything the publisher asserts, so
    /// a publisher cannot attribute a terminal to somebody who did not ask for one.
    pub fn open_request_principal(&self, device_id: Uuid, request_id: &str) -> Option<String> {
        self.pending_opens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id, request_id.to_string()))
            .map(|entry| entry.principal_id.clone())
    }

    /// Fails every request outstanding for a device. Called the moment its publisher
    /// goes away or is superseded: a request is never queued for a device that is not
    /// connected (spec §4.6 condition 4).
    pub fn fail_open_requests_for_device(&self, device_id: Uuid) {
        let mut pending = self.pending_opens.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<_> = pending
            .keys()
            .filter(|(id, _)| *id == device_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(entry) = pending.remove(&key) {
                let _ = entry.tx.send(Some(OpenOutcome::Unavailable));
            }
        }
        metrics::TERMINAL_OPEN_REQUESTS_PENDING.set(pending.len() as i64);
    }

    /// Drops entries whose caller has long since given up, so a publisher that never
    /// answers cannot leak the table.
    pub fn sweep_pending_opens(&self, max_age: Duration) {
        let mut pending = self.pending_opens.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, entry| entry.created.elapsed() < max_age);
        metrics::TERMINAL_OPEN_REQUESTS_PENDING.set(pending.len() as i64);
    }

    /// Hand one input frame to the device's publisher connection.
    ///
    /// Non-blocking by design: input is never queued for a disconnected publisher and
    /// never buffered beyond the connection's bounded channel, so a subscriber always
    /// learns promptly whether its keystrokes were delivered (spec §6.3).
    pub fn deliver_to_publisher(
        &self,
        device_id: Uuid,
        delivery: PublisherDelivery,
    ) -> Result<(), InputDeliveryError> {
        let publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = publishers.get(&device_id) else {
            return Err(InputDeliveryError::NoPublisher);
        };
        let Some(input_tx) = slot.input_tx.as_ref() else {
            return Err(InputDeliveryError::NoPublisher);
        };
        match input_tx.try_send(delivery) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                Err(InputDeliveryError::Backpressure)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(InputDeliveryError::NoPublisher)
            }
        }
    }

    /// Enforce revocation on live connections (spec §5.2: within 30 seconds).
    pub async fn enforce_device_revocation(self: &Arc<Self>, device_id: Uuid) {
        let superseded = {
            let mut publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
            publishers.remove(&device_id)
        };
        if let Some(slot) = superseded {
            metrics::REVOCATION_ENFORCED_DISCONNECTS.inc();
            slot.supersede.notify_waiters();
        }
        self.fail_open_requests_for_device(device_id);

        // Mirror subscriptions belong to the owning identity, which has not been
        // revoked, so they are not torn down. Closing the terminals delivers an
        // ordinary `terminal.closed` after the final commit, and the owner may still
        // replay the closed terminal within its retention window.
        if let Err(e) = self
            .close_device_terminals(device_id, "device_revoked")
            .await
        {
            tracing::error!(
                event = "revocation_close_failed",
                device_id = %device_id,
                error = %e,
                "could not close terminals for a revoked device"
            );
        }
    }

    // ------------------------------------------------------------------- terminals

    /// Open a terminal, or return the existing one for the same active `local_ref`.
    pub async fn open_terminal(
        self: &Arc<Self>,
        device: &Device,
        request: OpenRequest,
    ) -> ApiResult<(Arc<TerminalHandle>, bool)> {
        let capacity = self.settings.snapshot().replay_capacity();
        let max_terminals = self
            .settings
            .snapshot()
            .int(keys::LIMITS_MAX_ACTIVE_TERMINALS_PER_DEVICE);

        let device_id = device.device_id;
        let identity_id = device.identity_id.clone();
        let db = self.db.clone();

        let (row, deduplicated) = db
            .call(move |conn| {
                if let Some(existing) =
                    repo::find_open_terminal_by_local_ref(conn, device_id, &request.local_ref)?
                {
                    return Ok((existing, true));
                }
                let open_count = repo::count_open_terminals_for_device(conn, device_id)?;
                if open_count >= max_terminals {
                    return Err(ApiError::limit_exceeded(format!(
                        "device already has {open_count} open terminals, the limit is {max_terminals}"
                    )));
                }
                let inserted = in_txn(conn, |txn| {
                    repo::insert_terminal(
                        txn,
                        device_id,
                        &identity_id,
                        &request.local_ref,
                        &request.label,
                        request.cols,
                        request.rows,
                        request.term.as_deref(),
                        request.process_label.as_deref(),
                        request.accepts_input,
                        request.origin.as_str(),
                        request.requested_by_principal.as_deref(),
                    )
                });

                match inserted {
                    Ok(row) => Ok((row, false)),
                    Err(e) => {
                        // Two opens for the same active local_ref can race past the
                        // lookup above; the partial unique index is the arbiter, and
                        // the loser resolves to the winner's terminal so the operation
                        // stays idempotent rather than surfacing a conflict.
                        match repo::find_open_terminal_by_local_ref(
                            conn,
                            device_id,
                            &request.local_ref,
                        )? {
                            Some(existing) => Ok((existing, true)),
                            None => Err(e),
                        }
                    }
                }
            })
            .await?;

        let handle = self.insert_or_load_handle(&row, capacity).await?;

        if deduplicated {
            // A repeated open refreshes the terminal's advertised metadata.
            handle.update_metadata(
                Some(row.label.clone()),
                row.cols,
                row.rows,
                row.term.clone(),
            );
        } else {
            metrics::TERMINALS_OPENED_TOTAL.inc();
            metrics::TERMINALS_OPEN.inc();
        }

        Ok((handle, deduplicated))
    }

    async fn insert_or_load_handle(
        &self,
        row: &TerminalRow,
        capacity: usize,
    ) -> ApiResult<Arc<TerminalHandle>> {
        if let Some(handle) = self.resident(row.terminal_id) {
            return Ok(handle);
        }
        let _guard = self.load_lock.lock().await;
        if let Some(handle) = self.resident(row.terminal_id) {
            return Ok(handle);
        }

        let terminal_id = row.terminal_id;
        let earliest = row.earliest_offset;
        let db = self.db.clone();
        let retained = db
            .call(move |conn| repo::load_terminal_output(conn, terminal_id, earliest))
            .await?;

        let handle = Arc::new(TerminalHandle::from_row(row, capacity, retained));
        self.terminals
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(row.terminal_id, Arc::clone(&handle));
        Ok(handle)
    }

    pub fn resident(&self, terminal_id: Uuid) -> Option<Arc<TerminalHandle>> {
        self.terminals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&terminal_id)
            .cloned()
    }

    /// Fetch a terminal, loading its durable state into memory if necessary.
    pub async fn get_or_load(&self, terminal_id: Uuid) -> ApiResult<Option<Arc<TerminalHandle>>> {
        if let Some(handle) = self.resident(terminal_id) {
            return Ok(Some(handle));
        }
        let capacity = self.settings.snapshot().replay_capacity();
        let db = self.db.clone();
        let row = db
            .call(move |conn| repo::get_terminal(conn, terminal_id))
            .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(self.insert_or_load_handle(&row, capacity).await?))
    }

    pub fn resident_handles(&self) -> Vec<Arc<TerminalHandle>> {
        self.terminals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Drop retired terminals with no subscribers from the resident set, and forget
    /// terminals deleted by retention.
    pub fn evict_retired(&self) {
        let mut removed = Vec::new();
        {
            let mut terminals = self.terminals.write().unwrap_or_else(|e| e.into_inner());
            terminals.retain(|_, handle| {
                let evictable = handle.is_retired()
                    && handle.subscriber_count() == 0
                    && handle.dirty_bytes() == 0;
                if evictable {
                    removed.push(Arc::clone(handle));
                }
                !evictable
            });
        }
        for handle in &removed {
            handle.on_evict();
        }
        if !removed.is_empty() {
            tracing::debug!(event = "terminals_evicted", count = removed.len());
        }
    }

    pub fn forget(&self, terminal_id: Uuid) {
        let removed = self
            .terminals
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&terminal_id);
        if let Some(handle) = removed {
            handle.on_evict();
        }
    }

    // --------------------------------------------------------------- checkpointing

    pub fn request_flush(&self) {
        self.flush_request.notify_one();
    }

    pub async fn wait_flush_request(&self) {
        self.flush_request.notified().await;
    }

    pub fn dirty_stats(&self) -> DirtyStats {
        let mut total = 0u64;
        let mut oldest: Option<Duration> = None;
        let mut has_pending_close = false;

        for handle in self.resident_handles() {
            let dirty = handle.dirty_bytes();
            total += dirty;
            if let Some(since) = handle.dirty_since() {
                let age = since.elapsed();
                oldest = Some(oldest.map_or(age, |current: Duration| current.max(age)));
            }
            if matches!(handle.lifecycle(), Lifecycle::Closing(_)) {
                has_pending_close = true;
            }
        }

        metrics::DIRTY_BYTES.set(total as i64);
        metrics::DURABLE_OFFSET_LAG_BYTES.set(total as i64);

        DirtyStats {
            total_dirty_bytes: total,
            oldest_dirty: oldest,
            has_pending_close,
            lag_bytes: total,
        }
    }

    /// Terminate every subscriber and publisher, used during graceful shutdown.
    pub fn terminate_all(&self, termination: Termination) {
        for handle in self.resident_handles() {
            handle.terminate_subscribers(termination.clone());
        }
        let publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
        for slot in publishers.values() {
            slot.supersede.notify_waiters();
        }
    }

    /// Restore in-memory state for terminals left open by a previous process.
    ///
    /// Their publishers are not connected yet, so each starts a grace period; a
    /// publisher that reconnects and re-opens the same `local_ref` cancels it.
    pub async fn recover_open_terminals(self: &Arc<Self>) -> ApiResult<usize> {
        let db = self.db.clone();
        let rows = db.call(move |conn| repo::list_open_terminals(conn)).await?;
        let count = rows.len();
        metrics::TERMINALS_OPEN.set(count as i64);

        let mut devices: Vec<Uuid> = rows.iter().map(|r| r.device_id).collect();
        devices.sort_unstable();
        devices.dedup();

        for device_id in devices {
            let registry = Arc::clone(self);
            // Reuse the detach path so the grace period and its cancellation
            // behaviour are identical to a live disconnect.
            let generation = self.next_generation.fetch_add(1, Ordering::AcqRel);
            {
                let mut publishers = self.publishers.lock().unwrap_or_else(|e| e.into_inner());
                publishers.insert(
                    device_id,
                    PublisherSlot {
                        generation,
                        connection_id: "recovered".to_string(),
                        supersede: Arc::new(Notify::new()),
                        version: ProtocolVersion::V1,
                        input_tx: None,
                        // This slot stands in for a device that is *not* connected; it
                        // exists only to reuse the disconnect grace path. Anything that
                        // reads it as a live, capable publisher would be asking a
                        // machine that is not there.
                        open_requests: false,
                    },
                );
            }
            tokio::spawn(async move {
                registry.on_publisher_detached(device_id, generation).await;
            });
        }

        if count > 0 {
            tracing::info!(
                event = "terminals_recovered",
                count,
                "recovered open terminals from durable state"
            );
        }
        Ok(count)
    }
}
