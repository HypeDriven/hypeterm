-- Durable schema for the Terminal Mirror Relay.
--
-- Applied idempotently at startup. All statements are CREATE ... IF NOT EXISTS so
-- reopening an existing database is a no-op.

CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ------------------------------------------------------------------ identities

CREATE TABLE IF NOT EXISTS identities (
    identity_id TEXT PRIMARY KEY,          -- canonical fingerprint (spec §3.1)
    algorithm   TEXT NOT NULL,
    public_key  BLOB NOT NULL,
    created_at  TEXT NOT NULL,
    -- Re-registering the same canonical key must return the same identity.
    UNIQUE (algorithm, public_key)
);

-- ---------------------------------------------------------------------- devices

CREATE TABLE IF NOT EXISTS devices (
    device_id       TEXT PRIMARY KEY,      -- server-generated UUID
    identity_id     TEXT NOT NULL REFERENCES identities (identity_id),
    algorithm       TEXT NOT NULL,
    public_key      BLOB NOT NULL,
    key_fingerprint TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,         -- owner-scoped, non-unique
    -- publisher | client | both (spec §3.2). Defaults to publisher so a
    -- registration that omits the field keeps its version 1 meaning.
    role            TEXT NOT NULL DEFAULT 'publisher'
                        CHECK (role IN ('publisher', 'client', 'both')),
    created_at      TEXT NOT NULL,
    last_seen_at    TEXT,
    revoked_at      TEXT
);

CREATE INDEX IF NOT EXISTS devices_by_identity
    ON devices (identity_id, created_at, device_id);

-- -------------------------------------------------------------------- terminals

-- `durable_offset` is the offset immediately after the last byte committed here.
-- There is deliberately no stored `next_offset`: on disk the two are identical, and
-- after a restart the in-memory next_offset is rebuilt as durable_offset (spec §7.2).
CREATE TABLE IF NOT EXISTS terminals (
    terminal_id      TEXT PRIMARY KEY,     -- server-generated UUID
    device_id        TEXT NOT NULL REFERENCES devices (device_id),
    identity_id      TEXT NOT NULL,        -- denormalised owner, for ownership checks
    label            TEXT NOT NULL,
    local_ref        TEXT NOT NULL,        -- opaque device-local reference
    state            TEXT NOT NULL CHECK (state IN ('open', 'closed')),
    cols             INTEGER,
    rows             INTEGER,
    term             TEXT,
    process_label    TEXT,
    -- The publishing device's opt-in to receiving terminal input (spec §4.5).
    accepts_input    INTEGER NOT NULL DEFAULT 0,
    -- How this terminal came to exist: 'publisher' when the machine's own owner
    -- started it, 'request' when a subscriber asked for it (spec §4.6). Set by the
    -- relay from its own pending request, never from anything the publisher asserts.
    origin           TEXT NOT NULL DEFAULT 'publisher' CHECK (origin IN ('publisher', 'request')),
    requested_by_principal TEXT,
    created_at       TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    closed_at        TEXT,
    close_reason     TEXT,
    durable_offset   INTEGER NOT NULL DEFAULT 0,
    earliest_offset  INTEGER NOT NULL DEFAULT 0,
    CHECK (earliest_offset <= durable_offset)
);

-- Enforces terminal.open idempotency per device while a terminal is open, and
-- allows the same local_ref to be reused by a later session once closed.
CREATE UNIQUE INDEX IF NOT EXISTS terminals_active_local_ref
    ON terminals (device_id, local_ref) WHERE state = 'open';

CREATE INDEX IF NOT EXISTS terminals_by_identity
    ON terminals (identity_id, created_at, terminal_id);

CREATE INDEX IF NOT EXISTS terminals_by_device
    ON terminals (device_id, created_at, terminal_id);

CREATE INDEX IF NOT EXISTS terminals_closed_retention
    ON terminals (state, closed_at);

-- Replay payload, stored as append-only chunks so a checkpoint appends each byte
-- once instead of rewriting the whole retained suffix (spec §7.2).
CREATE TABLE IF NOT EXISTS terminal_output (
    terminal_id  TEXT NOT NULL REFERENCES terminals (terminal_id) ON DELETE CASCADE,
    start_offset INTEGER NOT NULL,
    end_offset   INTEGER NOT NULL,
    bytes        BLOB NOT NULL,
    UNIQUE (terminal_id, start_offset)
);

CREATE INDEX IF NOT EXISTS terminal_output_range
    ON terminal_output (terminal_id, end_offset);

-- ------------------------------------------------------- proof of possession

CREATE TABLE IF NOT EXISTS challenges (
    challenge_id           TEXT PRIMARY KEY,
    operation              TEXT NOT NULL,
    algorithm              TEXT NOT NULL,
    public_key             BLOB NOT NULL,
    key_fingerprint        TEXT NOT NULL,
    owner_identity_id      TEXT,           -- bound for register_device
    device_key_fingerprint TEXT,           -- bound for register_device
    challenge              BLOB NOT NULL,
    created_at             TEXT NOT NULL,
    expires_at             TEXT NOT NULL,
    -- Set on the first verification attempt, successful or not (spec §4.2).
    consumed_at            TEXT
);

CREATE INDEX IF NOT EXISTS challenges_expiry ON challenges (expires_at);

CREATE TABLE IF NOT EXISTS websocket_tickets (
    ticket_hash    TEXT PRIMARY KEY,       -- only the hash is stored
    principal_kind TEXT NOT NULL,
    principal_id   TEXT NOT NULL,
    identity_id    TEXT NOT NULL,
    path           TEXT NOT NULL,          -- ticket is valid for exactly one path
    scopes         TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL,
    consumed_at    TEXT
);

CREATE INDEX IF NOT EXISTS websocket_tickets_expiry ON websocket_tickets (expires_at);

-- Tokens are stateless; this records the exceptions that must be rejected early.
CREATE TABLE IF NOT EXISTS revoked_tokens (
    jti        TEXT PRIMARY KEY,
    expires_at TEXT NOT NULL
);

-- Principal-level cutoff: any token issued at or before `not_before` is rejected.
-- Used by device revocation to invalidate already-issued tokens.
CREATE TABLE IF NOT EXISTS principal_token_cutoffs (
    principal_id TEXT PRIMARY KEY,
    not_before   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS signing_keys (
    kid        TEXT PRIMARY KEY,
    nonce      BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL,
    active     INTEGER NOT NULL DEFAULT 0,
    -- End of the rotation overlap window; after this the key stops verifying.
    not_after  TEXT
);

-- ---------------------------------------------------------------------- settings

CREATE TABLE IF NOT EXISTS settings (
    name       TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    revision   INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings_audit (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    revision       INTEGER NOT NULL,
    at             TEXT NOT NULL,
    operator       TEXT NOT NULL,
    setting        TEXT NOT NULL,
    old_value_hash TEXT,                   -- hashes only; never raw secrets
    new_value_hash TEXT,
    outcome        TEXT NOT NULL,
    detail         TEXT
);

CREATE INDEX IF NOT EXISTS settings_audit_at ON settings_audit (at);

-- ------------------------------------------------------------------ idempotency

CREATE TABLE IF NOT EXISTS idempotency (
    key_hash      TEXT PRIMARY KEY,
    principal_id  TEXT NOT NULL,
    method        TEXT NOT NULL,
    path          TEXT NOT NULL,
    request_hash  TEXT NOT NULL,
    status        INTEGER NOT NULL,
    response_body TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idempotency_created ON idempotency (created_at);

-- Counts identity registrations per source address for the spec §10 limit.
CREATE TABLE IF NOT EXISTS registration_events (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS registration_events_source ON registration_events (source, at);
