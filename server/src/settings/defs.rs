//! The setting registry: the single source of truth for every behaviour-driving
//! value in the service (spec §8.1).
//!
//! The `settings!` macro below generates, from one declaration per setting:
//!   * `keys::NAME` string constants used at every call site,
//!   * the `DEFS` metadata table served by `GET /v1/admin/settings`, and
//!   * `ALL_KEYS`, used by a startup self-check.
//!
//! Code must never read a behaviour value from anywhere else. Constants in code are
//! permitted only as the seeded defaults and hard safety bounds declared here.

use super::{DefaultValue, Kind, Reload, SettingDef};

/// Hard specification ceiling for the per-terminal replay window (spec §7.1).
/// Decimal 1.5 MB, not 1.5 MiB. No database update may exceed this.
pub const REPLAY_CAPACITY_HARD_MAX: i64 = 1_500_000;

/// Bumped when the meaning or set of settings changes incompatibly. A database
/// carrying an unsupported version fails readiness rather than guessing (spec §8.1).
pub const SETTINGS_SCHEMA_VERSION: i64 = 1;

macro_rules! setting_kind {
    (Bool($($t:tt)*)) => {
        Kind::Bool
    };
    (Int($($t:tt)*)) => {
        Kind::Int
    };
    (Str($($t:tt)*)) => {
        Kind::Str
    };
    (Enum($($t:tt)*)) => {
        Kind::Enum
    };
    (List($($t:tt)*)) => {
        Kind::List
    };
    (Secret($($t:tt)*)) => {
        Kind::Secret
    };
}

macro_rules! setting_default {
    (Bool($d:expr)) => { DefaultValue::Bool($d) };
    (Int($d:expr, $min:expr, $max:expr)) => { DefaultValue::Int($d) };
    (Str($d:expr)) => { DefaultValue::Str($d) };
    (Enum($d:expr, [$($v:expr),* $(,)?])) => { DefaultValue::Str($d) };
    (List([$($v:expr),* $(,)?])) => { DefaultValue::List(&[$($v),*]) };
    (Secret($d:expr)) => { DefaultValue::Str($d) };
}

macro_rules! setting_min {
    (Int($d:expr, $min:expr, $max:expr)) => {
        Some($min)
    };
    ($($t:tt)*) => {
        None
    };
}

macro_rules! setting_max {
    (Int($d:expr, $min:expr, $max:expr)) => {
        Some($max)
    };
    ($($t:tt)*) => {
        None
    };
}

macro_rules! setting_allowed {
    (Enum($d:expr, [$($v:expr),* $(,)?])) => { Some(&[$($v),*]) };
    ($($t:tt)*) => { None };
}

macro_rules! setting_secret {
    (Secret($($t:tt)*)) => {
        true
    };
    ($($t:tt)*) => {
        false
    };
}

macro_rules! settings {
    ($(
        $cname:ident = $name:literal : $kind:ident ( $($args:tt)* ),
            reload: $reload:ident,
            desc: $desc:literal ;
    )*) => {
        /// Stable dotted setting names, for use at call sites.
        pub mod keys {
            $( pub const $cname: &str = $name; )*
        }

        pub static DEFS: &[SettingDef] = &[ $(
            SettingDef {
                name: $name,
                kind: setting_kind!($kind($($args)*)),
                default: setting_default!($kind($($args)*)),
                min: setting_min!($kind($($args)*)),
                max: setting_max!($kind($($args)*)),
                allowed: setting_allowed!($kind($($args)*)),
                secret: setting_secret!($kind($($args)*)),
                reload: Reload::$reload,
                description: $desc,
            }
        ),* ];

        pub static ALL_KEYS: &[&str] = &[ $( $name ),* ];
    };
}

settings! {
    // ---------------------------------------------------------------- server / network
    SERVER_LISTEN_ADDRESS = "server.listen_address": Str("0.0.0.0:8080"),
        reload: ListenerRebind,
        desc: "Socket address for the main API and WebSocket listener. Changing it rebinds the listener and drains the previous one.";
    SERVER_HEALTH_LISTEN_ADDRESS = "server.health_listen_address": Str(""),
        reload: ListenerRebind,
        desc: "Optional isolated plain-HTTP listener serving only /healthz and /readyz. Empty disables it.";
    SERVER_TLS_ENABLED = "server.tls_enabled": Bool(false),
        reload: ListenerRebind,
        desc: "Terminate TLS in-process. When false the service must run behind a trusted TLS-terminating proxy in production.";
    SERVER_TLS_CERTIFICATE_PATH = "server.tls_certificate_path": Str(""),
        reload: ListenerRebind,
        desc: "PEM certificate chain path, required when server.tls_enabled is true.";
    SERVER_TLS_PRIVATE_KEY_PATH = "server.tls_private_key_path": Str(""),
        reload: ListenerRebind,
        desc: "PEM private key path, required when server.tls_enabled is true.";
    SERVER_PUBLIC_ORIGIN = "server.public_origin": Str("http://localhost:8080"),
        reload: Immediate,
        desc: "Canonical public origin. Bound into every proof-of-possession signing input, so changing it invalidates outstanding challenges.";
    SERVER_SHUTDOWN_DEADLINE_SECONDS = "server.shutdown_deadline_seconds": Int(30, 1, 600),
        reload: Immediate,
        desc: "Maximum time to finish committed writes and close connections after SIGTERM.";
    SERVER_CONNECTION_DRAIN_SECONDS = "server.connection_drain_seconds": Int(10, 0, 600),
        reload: ListenerRebind,
        desc: "Grace period granted to connections on a superseded listener during a rebind.";
    SERVER_REQUEST_TIMEOUT_SECONDS = "server.request_timeout_seconds": Int(30, 1, 600),
        reload: Immediate,
        desc: "Maximum duration of a single non-WebSocket HTTP request.";
    SERVER_MAX_CONCURRENT_CONNECTIONS = "server.max_concurrent_connections": Int(4096, 1, 1_000_000),
        reload: Immediate,
        desc: "Upper bound on simultaneously accepted TCP connections across all principals.";

    // ---------------------------------------------------------------- transport security
    SECURITY_REQUIRE_SECURE_TRANSPORT = "security.require_secure_transport": Bool(true),
        reload: Immediate,
        desc: "Reject requests that did not arrive over TLS, either terminated here or asserted by a trusted proxy.";
    SECURITY_ALLOW_INSECURE_LOOPBACK = "security.allow_insecure_loopback": Bool(true),
        reload: Immediate,
        desc: "Exempt loopback peers from security.require_secure_transport. Intended for development; disable in production.";
    SECURITY_TRUSTED_PROXY_ENABLED = "security.trusted_proxy_enabled": Bool(false),
        reload: Immediate,
        desc: "Honour forwarded client address and protocol headers from peers inside security.trusted_proxy_networks.";
    SECURITY_TLS_TERMINATED_BY_NETWORKS = "security.tls_terminated_by_networks": List([]),
        reload: Immediate,
        desc: "CIDR blocks whose inbound connections have already had TLS terminated by a trusted party that forwards raw TCP, and so satisfy security.require_secure_transport without a forwarded-protocol header. Intended for a co-located terminator such as a Tailscale sidecar; an entry here fully trusts anything that can reach the listener from that range.";
    SECURITY_TRUSTED_PROXY_NETWORKS = "security.trusted_proxy_networks": List([]),
        reload: Immediate,
        desc: "CIDR blocks whose forwarded headers are trusted. Headers from any other peer are ignored.";
    SECURITY_FORWARDED_FOR_HEADER = "security.forwarded_for_header": Str("x-forwarded-for"),
        reload: Immediate,
        desc: "Header carrying the originating client address, honoured only from a trusted proxy.";
    SECURITY_FORWARDED_PROTO_HEADER = "security.forwarded_proto_header": Str("x-forwarded-proto"),
        reload: Immediate,
        desc: "Header carrying the original scheme, honoured only from a trusted proxy.";
    SECURITY_REVOCATION_RECHECK_SECONDS = "security.revocation_recheck_seconds": Int(10, 1, 30),
        reload: Immediate,
        desc: "How often a live WebSocket re-checks its principal against durable revocation state. Capped at the thirty seconds the specification allows for terminating existing access.";

    // ---------------------------------------------------------------- authentication
    AUTH_SUPPORTED_KEY_ALGORITHMS = "auth.supported_key_algorithms": List(["ed25519"]),
        reload: Immediate,
        desc: "Public-key algorithms accepted for identity and device keys.";
    AUTH_CHALLENGE_TTL_SECONDS = "auth.challenge_ttl_seconds": Int(300, 5, 300),
        reload: Immediate,
        desc: "Proof-of-possession challenge lifetime. The specification caps this at five minutes.";
    AUTH_CHALLENGE_BYTES = "auth.challenge_bytes": Int(32, 32, 128),
        reload: Immediate,
        desc: "Random bytes per challenge. The specification requires at least 32.";
    AUTH_ACCESS_TOKEN_TTL_SECONDS = "auth.access_token_ttl_seconds": Int(900, 30, 900),
        reload: Immediate,
        desc: "Access token lifetime. The specification caps this at fifteen minutes.";
    AUTH_WEBSOCKET_TICKET_TTL_SECONDS = "auth.websocket_ticket_ttl_seconds": Int(60, 5, 60),
        reload: Immediate,
        desc: "Single-use WebSocket ticket lifetime. The specification caps this at sixty seconds.";
    AUTH_IDENTITY_TOKEN_SCOPES = "auth.identity_token_scopes": List(["devices:read", "devices:write", "terminals:read", "terminals:mirror", "terminals:input"]),
        reload: Immediate,
        desc: "Scopes granted to identity access tokens.";
    AUTH_DEVICE_TOKEN_SCOPES = "auth.device_token_scopes": List(["terminals:write", "terminals:publish"]),
        reload: Immediate,
        desc: "Scopes granted to publisher-role device tokens. Must not include identity-level or input scopes.";
    AUTH_CLIENT_TOKEN_SCOPES = "auth.client_token_scopes": List(["terminals:read", "terminals:mirror", "terminals:input"]),
        reload: Immediate,
        desc: "Scopes granted to client-role device tokens, which mirror and write to their owner's terminals but never manage devices or publish.";
    AUTH_SIGNING_KEY_ROTATION_SECONDS = "auth.signing_key_rotation_seconds": Int(2_592_000, 3600, 31_536_000),
        reload: Immediate,
        desc: "Age at which the token signing key is rotated.";
    AUTH_SIGNING_KEY_OVERLAP_SECONDS = "auth.signing_key_overlap_seconds": Int(3600, 60, 86_400),
        reload: Immediate,
        desc: "Period after rotation during which the previous signing key still verifies existing tokens.";
    AUTH_MAX_CLOCK_SKEW_SECONDS = "auth.max_clock_skew_seconds": Int(5, 0, 60),
        reload: Immediate,
        desc: "Tolerance applied to token and challenge expiry comparisons.";
    AUTH_OPERATOR_TOKEN_HASH = "auth.operator_token_hash": Secret(""),
        reload: Immediate,
        desc: "SHA-256 hash of the operator bearer token for /v1/admin and /metrics. Operator authentication is separate from identity and device authentication.";

    // ---------------------------------------------------------------- feature switches
    FEATURES_IDENTITY_REGISTRATION_ENABLED = "features.identity_registration_enabled": Bool(true),
        reload: Immediate,
        desc: "Allow self-service identity registration.";
    FEATURES_DEVICE_REGISTRATION_ENABLED = "features.device_registration_enabled": Bool(true),
        reload: Immediate,
        desc: "Allow new device registration.";
    FEATURES_PUBLISH_ENABLED = "features.publish_enabled": Bool(true),
        reload: Immediate,
        desc: "Accept publisher relay connections.";
    FEATURES_MIRROR_ENABLED = "features.mirror_enabled": Bool(true),
        reload: Immediate,
        desc: "Accept mirror subscriber connections.";
    FEATURES_METRICS_ENDPOINT_ENABLED = "features.metrics_endpoint_enabled": Bool(true),
        reload: Immediate,
        desc: "Expose GET /metrics.";
    FEATURES_INPUT_ENABLED = "features.input_enabled": Bool(true),
        reload: Immediate,
        desc: "Deliver terminal input from subscribers to publishers. A security control, not a negotiated limit: turning it off stops input on existing connections at once. Input still requires the publisher's per-terminal opt-in.";
    FEATURES_CLIENT_RESIZE_ENABLED = "features.client_resize_enabled": Bool(true),
        reload: Immediate,
        desc: "Allow a subscriber with input authority to ask the publisher to resize a terminal. The publisher remains the sole authority over the dimensions.";
    FEATURES_TERMINAL_CREATE_ENABLED = "features.terminal_create_enabled": Bool(false),
        reload: Immediate,
        desc: "Allow a subscriber to ask a publishing device to open a terminal. A security control, not a negotiated limit: turning it off stops new requests on existing connections at once. Off by default so upgrading the server never grants remote process creation; creation also requires the publishing device's own opt-in and an explicitly granted terminals:create scope.";

    // ---------------------------------------------------------------- rate limits
    RATELIMIT_ENABLED = "ratelimit.enabled": Bool(true),
        reload: Immediate,
        desc: "Master switch for request rate limiting.";
    RATELIMIT_CHALLENGES_PER_MINUTE_PER_SOURCE = "ratelimit.challenges_per_minute_per_source": Int(30, 1, 100_000),
        reload: Immediate,
        desc: "Challenge creations per minute per source address.";
    RATELIMIT_CHALLENGES_PER_MINUTE_PER_FINGERPRINT = "ratelimit.challenges_per_minute_per_fingerprint": Int(10, 1, 100_000),
        reload: Immediate,
        desc: "Challenge creations per minute per public-key fingerprint.";
    RATELIMIT_IDENTITY_REGISTRATIONS_PER_HOUR_PER_SOURCE = "ratelimit.identity_registrations_per_hour_per_source": Int(10, 1, 100_000),
        reload: Immediate,
        desc: "Identity registrations per hour per source address (spec §10 limit on identities per source).";
    RATELIMIT_TOKEN_REQUESTS_PER_MINUTE_PER_FINGERPRINT = "ratelimit.token_requests_per_minute_per_fingerprint": Int(60, 1, 100_000),
        reload: Immediate,
        desc: "Token exchanges per minute per key fingerprint.";
    RATELIMIT_REQUESTS_PER_MINUTE_PER_PRINCIPAL = "ratelimit.requests_per_minute_per_principal": Int(600, 1, 10_000_000),
        reload: Immediate,
        desc: "Authenticated API requests per minute per principal.";
    RATELIMIT_WEBSOCKET_CONNECTIONS_PER_MINUTE_PER_PRINCIPAL = "ratelimit.websocket_connections_per_minute_per_principal": Int(60, 1, 100_000),
        reload: Immediate,
        desc: "WebSocket upgrades per minute per principal.";
    RATELIMIT_INPUT_FRAMES_PER_MINUTE_PER_SUBSCRIBER = "ratelimit.input_frames_per_minute_per_subscriber": Int(6000, 60, 1_000_000),
        reload: Immediate,
        desc: "Input frames per minute per mirror subscription. Generous enough for fast typing and key repeat, bounded against a flood.";
    RATELIMIT_TERMINAL_CREATES_PER_HOUR_PER_PRINCIPAL = "ratelimit.terminal_creates_per_hour_per_principal": Int(20, 1, 10_000),
        reload: Immediate,
        desc: "Terminal-open requests one principal may make per hour.";
    RATELIMIT_TERMINAL_CREATES_PER_HOUR_PER_DEVICE = "ratelimit.terminal_creates_per_hour_per_device": Int(20, 1, 10_000),
        reload: Immediate,
        desc: "Terminal-open requests one publishing device may be asked to serve per hour, whichever principal asks. This is the bucket that protects the target machine.";
    RATELIMIT_INPUT_BYTES_PER_MINUTE_PER_SUBSCRIBER = "ratelimit.input_bytes_per_minute_per_subscriber": Int(1_048_576, 4096, 104_857_600),
        reload: Immediate,
        desc: "Input payload bytes per minute per mirror subscription, which bounds paste volume.";
    RATELIMIT_RETRY_AFTER_SECONDS = "ratelimit.retry_after_seconds": Int(60, 1, 3600),
        reload: Immediate,
        desc: "Value advertised in the Retry-After header on 429 responses.";

    // ---------------------------------------------------------------- limits
    LIMITS_MAX_REQUEST_BODY_BYTES = "limits.max_request_body_bytes": Int(65_536, 1024, 10_485_760),
        reload: Immediate,
        desc: "Maximum accepted HTTP request body size.";
    LIMITS_MAX_DEVICES_PER_IDENTITY = "limits.max_devices_per_identity": Int(100, 1, 100_000),
        reload: Immediate,
        desc: "Maximum non-revoked devices one identity may own.";
    LIMITS_MAX_ACTIVE_TERMINALS_PER_DEVICE = "limits.max_active_terminals_per_device": Int(50, 1, 10_000),
        reload: Immediate,
        desc: "Maximum simultaneously open terminals per device.";
    LIMITS_MAX_CONNECTIONS_PER_PRINCIPAL = "limits.max_connections_per_principal": Int(20, 1, 10_000),
        reload: Immediate,
        desc: "Maximum concurrent WebSocket connections per identity or device.";
    LIMITS_MAX_OUTPUT_FRAME_BYTES = "limits.max_output_frame_bytes": Int(262_144, 1024, 8_388_608),
        reload: ConnectionRenegotiate,
        desc: "Maximum payload bytes per publisher output frame, advertised in ready. Existing connections keep the negotiated value, except that a reduction applies immediately to bound memory.";
    LIMITS_MAX_UNACKED_OUTPUT_BYTES = "limits.max_unacked_output_bytes": Int(4_194_304, 65_536, 268_435_456),
        reload: ConnectionRenegotiate,
        desc: "Maximum accepted-but-not-yet-durable bytes per terminal before a publisher is throttled then closed. Reductions apply immediately.";
    LIMITS_MAX_INPUT_FRAME_BYTES = "limits.max_input_frame_bytes": Int(4096, 64, 65_536),
        reload: ConnectionRenegotiate,
        desc: "Maximum payload bytes per subscriber input frame, advertised in ready. Sized for keystrokes and pastes, not bulk transfer.";
    LIMITS_MAX_INPUT_QUEUE_FRAMES = "limits.max_input_queue_frames": Int(256, 8, 65_536),
        reload: ConnectionRenegotiate,
        desc: "Input frames that may be queued to one publisher connection before further input is refused with input_backpressure.";
    LIMITS_MAX_CONTROL_MESSAGE_BYTES = "limits.max_control_message_bytes": Int(65_536, 512, 1_048_576),
        reload: ConnectionRenegotiate,
        desc: "Maximum size of a JSON control frame on either WebSocket protocol.";
    LIMITS_MAX_PENDING_OPEN_REQUESTS_PER_DEVICE = "limits.max_pending_open_requests_per_device": Int(4, 1, 64),
        reload: Immediate,
        desc: "Terminal-open requests that may be in flight to one device at once.";
    LIMITS_MAX_PENDING_OPEN_REQUESTS_TOTAL = "limits.max_pending_open_requests_total": Int(64, 1, 4096),
        reload: Immediate,
        desc: "Terminal-open requests that may be in flight across the whole service. Bounds what a burst of requests can hold open, which is the real protection rather than the timeout.";
    TERMINAL_OPEN_REQUEST_TIMEOUT_SECONDS = "terminal.open_request_timeout_seconds": Int(15, 1, 30),
        reload: Immediate,
        desc: "How long to wait for a publisher to answer a terminal-open request. Must stay below the client's own HTTP request timeout, or the caller gives up first and cannot learn the outcome.";
    LIMITS_MAX_LABEL_BYTES = "limits.max_label_bytes": Int(200, 1, 8192),
        reload: Immediate,
        desc: "Maximum length of a device-supplied terminal label.";
    LIMITS_MAX_LOCAL_REF_BYTES = "limits.max_local_ref_bytes": Int(128, 1, 8192),
        reload: Immediate,
        desc: "Maximum length of a device-local terminal reference.";
    LIMITS_MAX_TERM_BYTES = "limits.max_term_bytes": Int(64, 1, 1024),
        reload: Immediate,
        desc: "Maximum length of the TERM value.";
    LIMITS_MAX_DEVICE_NAME_BYTES = "limits.max_device_name_bytes": Int(200, 1, 8192),
        reload: Immediate,
        desc: "Maximum length of a device display name.";
    LIMITS_MAX_PROCESS_LABEL_BYTES = "limits.max_process_label_bytes": Int(200, 1, 8192),
        reload: Immediate,
        desc: "Maximum length of the optional host-local process label.";
    LIMITS_MAX_TERMINAL_COLS = "limits.max_terminal_cols": Int(10_000, 1, 1_000_000),
        reload: Immediate,
        desc: "Maximum accepted terminal column count.";
    LIMITS_MAX_TERMINAL_ROWS = "limits.max_terminal_rows": Int(10_000, 1, 1_000_000),
        reload: Immediate,
        desc: "Maximum accepted terminal row count.";
    LIMITS_MAX_PAGE_SIZE = "limits.max_page_size": Int(100, 1, 1000),
        reload: Immediate,
        desc: "Maximum page size for list endpoints.";
    LIMITS_DEFAULT_PAGE_SIZE = "limits.default_page_size": Int(50, 1, 1000),
        reload: Immediate,
        desc: "Default page size for list endpoints when the caller does not specify one.";

    // ---------------------------------------------------------------- terminals
    TERMINAL_REPLAY_CAPACITY_BYTES = "terminal.replay_capacity_bytes": Int(1_500_000, 1024, 1_500_000),
        reload: Immediate,
        desc: "Retained output bytes per terminal. Decimal 1.5 MB. May be tuned downward only; the schema maximum is the specification ceiling.";
    TERMINAL_PUBLISHER_RECONNECT_GRACE_SECONDS = "terminal.publisher_reconnect_grace_seconds": Int(60, 0, 86_400),
        reload: Immediate,
        desc: "How long a disconnected publisher's terminals stay open before closing with reason publisher_disconnected.";
    TERMINAL_CLOSED_RETENTION_SECONDS = "terminal.closed_retention_seconds": Int(86_400, 60, 31_536_000),
        reload: Immediate,
        desc: "How long closed terminals and their replay data are retained.";
    TERMINAL_ALLOW_PROCESS_LABEL = "terminal.allow_process_label": Bool(false),
        reload: Immediate,
        desc: "Accept the optional host-local process label. Off by default because it can leak process detail (spec §3.3).";

    // ---------------------------------------------------------------- persistence
    PERSISTENCE_FLUSH_INTERVAL_MS = "persistence.flush_interval_ms": Int(5000, 10, 60_000),
        reload: Immediate,
        desc: "Maximum age of the oldest dirty output byte before a checkpoint transaction is forced.";
    PERSISTENCE_FLUSH_BYTES = "persistence.flush_bytes": Int(262_144, 4096, 67_108_864),
        reload: Immediate,
        desc: "Total dirty output bytes that trigger a checkpoint transaction.";
    PERSISTENCE_MEMORY_PRESSURE_DIRTY_BYTES = "persistence.memory_pressure_dirty_bytes": Int(33_554_432, 65_536, 4_294_967_296),
        reload: Immediate,
        desc: "Process-wide dirty output bytes treated as memory pressure, forcing an immediate checkpoint.";
    PERSISTENCE_BACKPRESSURE_WAIT_MS = "persistence.backpressure_wait_ms": Int(5000, 10, 60_000),
        reload: Immediate,
        desc: "How long a publisher waits for a checkpoint when its unacknowledged window is full before the connection fails with storage_unavailable.";
    PERSISTENCE_COMMIT_RETRY_INITIAL_MS = "persistence.commit_retry_initial_ms": Int(50, 1, 60_000),
        reload: Immediate,
        desc: "Initial backoff after a failed checkpoint commit.";
    PERSISTENCE_COMMIT_RETRY_MAX_MS = "persistence.commit_retry_max_ms": Int(5000, 1, 300_000),
        reload: Immediate,
        desc: "Maximum backoff between checkpoint commit retries.";
    PERSISTENCE_COMMIT_RETRY_MAX_ATTEMPTS = "persistence.commit_retry_max_attempts": Int(10, 1, 1000),
        reload: Immediate,
        desc: "Consecutive failed checkpoint commits before readiness fails and publishers receive storage_unavailable.";
    PERSISTENCE_STORAGE_QUOTA_BYTES = "persistence.storage_quota_bytes": Int(5_000_000_000, 1_048_576, 1_000_000_000_000),
        reload: Immediate,
        desc: "Overall durable storage quota for retained terminal output.";
    PERSISTENCE_RETENTION_SWEEP_INTERVAL_SECONDS = "persistence.retention_sweep_interval_seconds": Int(300, 5, 86_400),
        reload: Immediate,
        desc: "Interval between retention and quota enforcement sweeps.";
    PERSISTENCE_SQLITE_SYNCHRONOUS = "persistence.sqlite_synchronous": Enum("normal", ["off", "normal", "full"]),
        reload: StorageReconfigure,
        desc: "SQLite synchronous mode. normal is durable across process crash with WAL; full also survives host power loss.";
    PERSISTENCE_SQLITE_WAL_AUTOCHECKPOINT_PAGES = "persistence.sqlite_wal_autocheckpoint_pages": Int(4000, 0, 1_000_000),
        reload: StorageReconfigure,
        desc: "WAL auto-checkpoint threshold in pages. Larger values reduce physical writes at the cost of WAL size.";
    PERSISTENCE_SQLITE_BUSY_TIMEOUT_MS = "persistence.sqlite_busy_timeout_ms": Int(5000, 0, 60_000),
        reload: StorageReconfigure,
        desc: "How long a database operation waits on a lock before failing.";
    PERSISTENCE_SQLITE_CACHE_SIZE_KIB = "persistence.sqlite_cache_size_kib": Int(8192, 64, 4_194_304),
        reload: StorageReconfigure,
        desc: "Per-connection SQLite page cache size.";

    // ---------------------------------------------------------------- mirror fan-out
    MIRROR_SUBSCRIBER_QUEUE_BYTES = "mirror.subscriber_queue_bytes": Int(4_194_304, 65_536, 268_435_456),
        reload: ConnectionRenegotiate,
        desc: "Outbound queue bound per subscriber. Exceeding it closes the subscriber as a slow consumer. Reductions apply immediately.";
    MIRROR_SUBSCRIBER_QUEUE_MESSAGES = "mirror.subscriber_queue_messages": Int(1024, 8, 1_048_576),
        reload: ConnectionRenegotiate,
        desc: "Outbound queued message bound per subscriber.";
    MIRROR_REPLAY_CHUNK_BYTES = "mirror.replay_chunk_bytes": Int(65_536, 1024, 1_048_576),
        reload: Immediate,
        desc: "Maximum payload per outbound replay frame. Replay is split into frames of at most this size.";

    // ---------------------------------------------------------------- websocket
    WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS = "websocket.heartbeat_interval_seconds": Int(20, 1, 3600),
        reload: ConnectionRenegotiate,
        desc: "Interval between server pings on both WebSocket protocols.";
    WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS = "websocket.heartbeat_timeout_seconds": Int(60, 2, 7200),
        reload: ConnectionRenegotiate,
        desc: "Silence after which an unresponsive WebSocket is closed. Must exceed the heartbeat interval.";
    WEBSOCKET_HANDSHAKE_TIMEOUT_SECONDS = "websocket.handshake_timeout_seconds": Int(10, 1, 300),
        reload: Immediate,
        desc: "Time allowed for the first control message after a WebSocket upgrade.";

    // ---------------------------------------------------------------- settings plumbing
    SETTINGS_PROPAGATION_INTERVAL_MS = "settings.propagation_interval_ms": Int(1000, 50, 60_000),
        reload: Immediate,
        desc: "How often an instance polls for a newer committed settings revision. Bounds convergence across instances.";

    // ---------------------------------------------------------------- observability
    LOGGING_LEVEL = "logging.level": Enum("info", ["trace", "debug", "info", "warn", "error"]),
        reload: LoggingReload,
        desc: "Minimum log level.";
    LOGGING_FORMAT = "logging.format": Enum("json", ["json", "text"]),
        reload: LoggingReload,
        desc: "Log encoding written to standard output.";
    METRICS_REQUIRE_OPERATOR_AUTH = "metrics.require_operator_auth": Bool(true),
        reload: Immediate,
        desc: "Require operator authentication on GET /metrics.";

    // ---------------------------------------------------------------- idempotency
    IDEMPOTENCY_ENABLED = "idempotency.enabled": Bool(true),
        reload: Immediate,
        desc: "Honour the Idempotency-Key header on mutating requests.";
    IDEMPOTENCY_RETENTION_SECONDS = "idempotency.retention_seconds": Int(172_800, 86_400, 2_592_000),
        reload: Immediate,
        desc: "How long idempotency records are retained. The specification requires at least 24 hours.";
}
