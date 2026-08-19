//! Process metrics (spec §9).
//!
//! Deliberately label-free: user-controlled labels and identity, device or terminal
//! IDs must never become metric dimensions, so every series here is either a plain
//! scalar or carries a small fixed-cardinality label such as a terminal state.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

pub struct Counter(AtomicU64);

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

pub struct Gauge(AtomicI64);

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn add(&self, v: i64) {
        self.0.fetch_add(v, Ordering::Relaxed);
    }
    pub fn sub(&self, v: i64) {
        self.0.fetch_sub(v, Ordering::Relaxed);
    }
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn dec(&self) {
        self.sub(1);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Fixed-bucket histogram; buckets are compile-time constants so cardinality is bounded.
pub struct Histogram {
    bounds: &'static [f64],
    buckets: [AtomicU64; 16],
    sum: AtomicU64, // micro-units, to stay integral
    count: AtomicU64,
}

impl Histogram {
    pub const fn new(bounds: &'static [f64]) -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            bounds,
            buckets: [ZERO; 16],
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, value: f64) {
        let idx = self
            .bounds
            .iter()
            .position(|b| value <= *b)
            .unwrap_or(self.bounds.len());
        if let Some(bucket) = self.buckets.get(idx) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum
            .fetch_add((value * 1_000_000.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        use std::fmt::Write;
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} histogram");
        let mut cumulative = 0u64;
        for (i, bound) in self.bounds.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
        }
        cumulative += self.buckets[self.bounds.len()].load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
        let sum = self.sum.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let _ = writeln!(out, "{name}_sum {sum}");
        let _ = writeln!(out, "{name}_count {}", self.count.load(Ordering::Relaxed));
    }
}

// ------------------------------------------------------------------- connections

pub static PUBLISHER_CONNECTIONS: Gauge = Gauge::new();
pub static MIRROR_CONNECTIONS: Gauge = Gauge::new();
pub static PUBLISHER_CONNECTIONS_TOTAL: Counter = Counter::new();
pub static MIRROR_CONNECTIONS_TOTAL: Counter = Counter::new();
pub static PUBLISHERS_SUPERSEDED: Counter = Counter::new();

// --------------------------------------------------------------------- terminals

pub static TERMINALS_OPEN: Gauge = Gauge::new();
pub static TERMINALS_OPENED_TOTAL: Counter = Counter::new();
pub static TERMINALS_CLOSED_TOTAL: Counter = Counter::new();
pub static TERMINALS_RESIDENT: Gauge = Gauge::new();

// Terminal-open requests (spec §4.6). Counted by outcome so an operator can see a
// machine being asked and refusing; no user-supplied value is ever a label.
pub static TERMINAL_OPEN_REQUESTS_PENDING: Gauge = Gauge::new();
pub static TERMINAL_OPEN_REQUESTS_OPENED: Counter = Counter::new();
pub static TERMINAL_OPEN_REQUESTS_DECLINED: Counter = Counter::new();
pub static TERMINAL_OPEN_REQUESTS_UNAVAILABLE: Counter = Counter::new();
pub static TERMINAL_OPEN_REQUESTS_TIMEOUT: Counter = Counter::new();
pub static TERMINAL_OPEN_REQUESTS_REFUSED: Counter = Counter::new();

// ------------------------------------------------------------------------- bytes

pub static OUTPUT_BYTES_ACCEPTED: Counter = Counter::new();
pub static OUTPUT_BYTES_DELIVERED: Counter = Counter::new();
pub static OUTPUT_BYTES_REPLAYED: Counter = Counter::new();
pub static REPLAY_BYTES_RESIDENT: Gauge = Gauge::new();
pub static DIRTY_BYTES: Gauge = Gauge::new();
pub static DURABLE_OFFSET_LAG_BYTES: Gauge = Gauge::new();
pub static EVICTED_BYTES: Counter = Counter::new();
pub static EVICTIONS: Counter = Counter::new();

// -------------------------------------------------------------------- checkpoints

pub static CHECKPOINT_TRANSACTIONS: Counter = Counter::new();
pub static CHECKPOINT_FAILURES: Counter = Counter::new();
pub static CHECKPOINT_ROWS_WRITTEN: Counter = Counter::new();
pub static CHECKPOINT_FRAMES_COALESCED: Counter = Counter::new();
pub static CHECKPOINT_BATCH_BYTES: Histogram = Histogram::new(&[
    4096.0,
    16384.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
]);
pub static CHECKPOINT_BATCH_AGE_SECONDS: Histogram =
    Histogram::new(&[0.01, 0.05, 0.25, 1.0, 5.0, 15.0, 60.0]);
pub static CHECKPOINT_TERMINALS_PER_BATCH: Histogram =
    Histogram::new(&[1.0, 2.0, 4.0, 8.0, 16.0, 64.0, 256.0]);

// -------------------------------------------------------------------- protocol

pub static OFFSET_MISMATCHES: Counter = Counter::new();
pub static OFFSET_AHEAD_REJECTIONS: Counter = Counter::new();
pub static REPLAY_GAPS: Counter = Counter::new();
pub static SLOW_CONSUMER_DISCONNECTS: Counter = Counter::new();
pub static OVERSIZED_FRAMES_REJECTED: Counter = Counter::new();
pub static BACKPRESSURE_WAITS: Counter = Counter::new();
pub static BACKPRESSURE_TIMEOUTS: Counter = Counter::new();

// ------------------------------------------------------------------------- input

pub static INPUT_FRAMES_DELIVERED: Counter = Counter::new();
pub static INPUT_BYTES_DELIVERED: Counter = Counter::new();
pub static INPUT_FRAMES_REFUSED: Counter = Counter::new();
pub static INPUT_RATE_LIMITED: Counter = Counter::new();
pub static RESIZE_REQUESTS_FORWARDED: Counter = Counter::new();

// ---------------------------------------------------------------------- security

pub static AUTH_FAILURES: Counter = Counter::new();
pub static AUTH_SUCCESSES: Counter = Counter::new();
pub static CHALLENGES_ISSUED: Counter = Counter::new();
pub static CHALLENGES_CONSUMED: Counter = Counter::new();
pub static RATE_LIMITED_REQUESTS: Counter = Counter::new();
pub static DEVICE_REVOCATIONS: Counter = Counter::new();
pub static REVOCATION_ENFORCED_DISCONNECTS: Counter = Counter::new();

// ----------------------------------------------------------------------- storage

pub static STORAGE_ERRORS: Counter = Counter::new();
pub static STORAGE_BYTES: Gauge = Gauge::new();
pub static RETENTION_TERMINALS_DELETED: Counter = Counter::new();
pub static QUOTA_TERMINALS_DELETED: Counter = Counter::new();
pub static STORAGE_UNAVAILABLE: Gauge = Gauge::new();

// ---------------------------------------------------------------------- settings

pub static SETTINGS_REVISION: Gauge = Gauge::new();
pub static SETTINGS_COMMITTED_REVISION: Gauge = Gauge::new();
pub static SETTINGS_RELOAD_FAILURES: Counter = Counter::new();
pub static LISTENER_REBINDS: Counter = Counter::new();

// ----------------------------------------------------------------------- requests

pub static HTTP_REQUESTS: Counter = Counter::new();
pub static HTTP_REQUESTS_FAILED: Counter = Counter::new();
pub static HTTP_REQUEST_SECONDS: Histogram =
    Histogram::new(&[0.001, 0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0]);

pub fn render() -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(8192);

    let mut gauge = |name: &str, help: &str, value: i64| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    };

    gauge(
        "relay_publisher_connections",
        "Active publisher relay connections.",
        PUBLISHER_CONNECTIONS.get(),
    );
    gauge(
        "relay_mirror_connections",
        "Active mirror subscriptions.",
        MIRROR_CONNECTIONS.get(),
    );
    gauge(
        "relay_terminals_open",
        "Terminals in the open state.",
        TERMINALS_OPEN.get(),
    );
    gauge(
        "relay_terminal_open_requests_pending",
        "Terminal-open requests awaiting a publisher's answer.",
        TERMINAL_OPEN_REQUESTS_PENDING.get(),
    );
    gauge(
        "relay_terminals_resident",
        "Terminals with an in-memory replay buffer.",
        TERMINALS_RESIDENT.get(),
    );
    gauge(
        "relay_replay_bytes_resident",
        "Output bytes held in in-memory replay buffers.",
        REPLAY_BYTES_RESIDENT.get(),
    );
    gauge(
        "relay_dirty_bytes",
        "Accepted output bytes not yet committed.",
        DIRTY_BYTES.get(),
    );
    gauge(
        "relay_durable_offset_lag_bytes",
        "Sum over terminals of next_offset minus durable_offset.",
        DURABLE_OFFSET_LAG_BYTES.get(),
    );
    gauge(
        "relay_storage_bytes",
        "Durable storage consumed, including WAL.",
        STORAGE_BYTES.get(),
    );
    gauge(
        "relay_storage_unavailable",
        "1 when durable storage is failing and readiness is degraded.",
        STORAGE_UNAVAILABLE.get(),
    );
    gauge(
        "relay_settings_revision",
        "Settings revision applied by this instance.",
        SETTINGS_REVISION.get(),
    );
    gauge(
        "relay_settings_committed_revision",
        "Latest settings revision committed to the database.",
        SETTINGS_COMMITTED_REVISION.get(),
    );
    gauge(
        "relay_settings_propagation_lag_revisions",
        "Committed settings revision minus the revision applied here.",
        SETTINGS_COMMITTED_REVISION.get() - SETTINGS_REVISION.get(),
    );

    let mut counter = |name: &str, help: &str, value: u64| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name} {value}");
    };

    counter(
        "relay_publisher_connections_total",
        "Publisher connections accepted.",
        PUBLISHER_CONNECTIONS_TOTAL.get(),
    );
    counter(
        "relay_mirror_connections_total",
        "Mirror subscriptions accepted.",
        MIRROR_CONNECTIONS_TOTAL.get(),
    );
    counter(
        "relay_publishers_superseded_total",
        "Publisher connections closed because a newer one took over the device.",
        PUBLISHERS_SUPERSEDED.get(),
    );
    counter(
        "relay_terminals_opened_total",
        "Terminals opened.",
        TERMINALS_OPENED_TOTAL.get(),
    );
    counter(
        "relay_terminals_closed_total",
        "Terminals closed.",
        TERMINALS_CLOSED_TOTAL.get(),
    );
    counter(
        "relay_terminal_open_requests_opened_total",
        "Terminal-open requests a publisher honoured.",
        TERMINAL_OPEN_REQUESTS_OPENED.get(),
    );
    counter(
        "relay_terminal_open_requests_declined_total",
        "Terminal-open requests a publisher refused.",
        TERMINAL_OPEN_REQUESTS_DECLINED.get(),
    );
    counter(
        "relay_terminal_open_requests_unavailable_total",
        "Terminal-open requests with no capable publisher connected.",
        TERMINAL_OPEN_REQUESTS_UNAVAILABLE.get(),
    );
    counter(
        "relay_terminal_open_requests_timeout_total",
        "Terminal-open requests a publisher never answered.",
        TERMINAL_OPEN_REQUESTS_TIMEOUT.get(),
    );
    counter(
        "relay_terminal_open_requests_refused_total",
        "Terminal-open requests the relay refused before asking anybody.",
        TERMINAL_OPEN_REQUESTS_REFUSED.get(),
    );
    counter(
        "relay_output_bytes_accepted_total",
        "Output payload bytes accepted from publishers.",
        OUTPUT_BYTES_ACCEPTED.get(),
    );
    counter(
        "relay_output_bytes_delivered_total",
        "Output payload bytes delivered to subscribers, live and replayed.",
        OUTPUT_BYTES_DELIVERED.get(),
    );
    counter(
        "relay_output_bytes_replayed_total",
        "Output payload bytes delivered from the replay window.",
        OUTPUT_BYTES_REPLAYED.get(),
    );
    counter(
        "relay_evicted_bytes_total",
        "Output bytes evicted from replay buffers.",
        EVICTED_BYTES.get(),
    );
    counter(
        "relay_evictions_total",
        "Replay buffer eviction operations.",
        EVICTIONS.get(),
    );
    counter(
        "relay_checkpoint_transactions_total",
        "Database transactions used to commit terminal output.",
        CHECKPOINT_TRANSACTIONS.get(),
    );
    counter(
        "relay_checkpoint_failures_total",
        "Failed checkpoint commits.",
        CHECKPOINT_FAILURES.get(),
    );
    counter(
        "relay_checkpoint_rows_written_total",
        "Rows written by checkpoint transactions.",
        CHECKPOINT_ROWS_WRITTEN.get(),
    );
    counter(
        "relay_checkpoint_frames_coalesced_total",
        "Publisher output frames folded into checkpoint transactions.",
        CHECKPOINT_FRAMES_COALESCED.get(),
    );
    counter(
        "relay_offset_mismatches_total",
        "Rejected publisher frames whose start offset did not match.",
        OFFSET_MISMATCHES.get(),
    );
    counter(
        "relay_offset_ahead_rejections_total",
        "Subscriptions rejected for requesting an offset beyond next_offset.",
        OFFSET_AHEAD_REJECTIONS.get(),
    );
    counter(
        "relay_replay_gaps_total",
        "Subscriptions served with a replay gap notice.",
        REPLAY_GAPS.get(),
    );
    counter(
        "relay_slow_consumer_disconnects_total",
        "Subscribers disconnected for exceeding their outbound queue bound.",
        SLOW_CONSUMER_DISCONNECTS.get(),
    );
    counter(
        "relay_oversized_frames_rejected_total",
        "Frames rejected for exceeding the negotiated frame size.",
        OVERSIZED_FRAMES_REJECTED.get(),
    );
    counter(
        "relay_backpressure_waits_total",
        "Times a publisher waited for a checkpoint before its frame was accepted.",
        BACKPRESSURE_WAITS.get(),
    );
    counter(
        "relay_backpressure_timeouts_total",
        "Times a publisher exceeded its unacknowledged window past the wait deadline.",
        BACKPRESSURE_TIMEOUTS.get(),
    );
    counter(
        "relay_input_frames_delivered_total",
        "Terminal input frames delivered to publishers.",
        INPUT_FRAMES_DELIVERED.get(),
    );
    counter(
        "relay_input_bytes_delivered_total",
        "Terminal input payload bytes delivered to publishers.",
        INPUT_BYTES_DELIVERED.get(),
    );
    counter(
        "relay_input_frames_refused_total",
        "Terminal input frames refused, for any reason.",
        INPUT_FRAMES_REFUSED.get(),
    );
    counter(
        "relay_input_rate_limited_total",
        "Terminal input frames refused by a per-subscriber rate limit.",
        INPUT_RATE_LIMITED.get(),
    );
    counter(
        "relay_resize_requests_forwarded_total",
        "Subscriber resize requests forwarded to publishers.",
        RESIZE_REQUESTS_FORWARDED.get(),
    );
    counter(
        "relay_auth_failures_total",
        "Failed authentication attempts.",
        AUTH_FAILURES.get(),
    );
    counter(
        "relay_auth_successes_total",
        "Successful authentications.",
        AUTH_SUCCESSES.get(),
    );
    counter(
        "relay_challenges_issued_total",
        "Proof-of-possession challenges issued.",
        CHALLENGES_ISSUED.get(),
    );
    counter(
        "relay_challenges_consumed_total",
        "Challenges consumed by a verification attempt.",
        CHALLENGES_CONSUMED.get(),
    );
    counter(
        "relay_rate_limited_requests_total",
        "Requests rejected by a rate limit.",
        RATE_LIMITED_REQUESTS.get(),
    );
    counter(
        "relay_device_revocations_total",
        "Devices revoked.",
        DEVICE_REVOCATIONS.get(),
    );
    counter(
        "relay_revocation_enforced_disconnects_total",
        "Connections closed because their principal was revoked.",
        REVOCATION_ENFORCED_DISCONNECTS.get(),
    );
    counter(
        "relay_storage_errors_total",
        "Durable storage errors.",
        STORAGE_ERRORS.get(),
    );
    counter(
        "relay_retention_terminals_deleted_total",
        "Closed terminals deleted after their retention period.",
        RETENTION_TERMINALS_DELETED.get(),
    );
    counter(
        "relay_quota_terminals_deleted_total",
        "Closed terminals deleted to satisfy the storage quota.",
        QUOTA_TERMINALS_DELETED.get(),
    );
    counter(
        "relay_settings_reload_failures_total",
        "Committed settings revisions that could not be applied.",
        SETTINGS_RELOAD_FAILURES.get(),
    );
    counter(
        "relay_listener_rebinds_total",
        "Listener rebinds performed.",
        LISTENER_REBINDS.get(),
    );
    counter(
        "relay_http_requests_total",
        "HTTP requests served.",
        HTTP_REQUESTS.get(),
    );
    counter(
        "relay_http_requests_failed_total",
        "HTTP requests answered with an error status.",
        HTTP_REQUESTS_FAILED.get(),
    );

    CHECKPOINT_BATCH_BYTES.render(
        &mut out,
        "relay_checkpoint_batch_bytes",
        "Output bytes per checkpoint transaction.",
    );
    CHECKPOINT_BATCH_AGE_SECONDS.render(
        &mut out,
        "relay_checkpoint_batch_age_seconds",
        "Age of the oldest dirty byte when a checkpoint committed.",
    );
    CHECKPOINT_TERMINALS_PER_BATCH.render(
        &mut out,
        "relay_checkpoint_terminals_per_batch",
        "Terminals coalesced into one checkpoint transaction.",
    );
    HTTP_REQUEST_SECONDS.render(
        &mut out,
        "relay_http_request_seconds",
        "HTTP request latency.",
    );

    out
}
