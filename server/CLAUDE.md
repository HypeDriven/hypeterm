# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`spec.md` is the normative source of truth: an RFC 2119 specification for the **Terminal Mirror Relay**, a containerized service that relays terminal output from registered devices to authenticated clients over WebSockets, and terminal input back. Treat every MUST in `spec.md` as a hard requirement and §11 (23 numbered acceptance criteria) as the test plan. `README.md` maps each criterion to the test that covers it.

The mirror is bidirectional as of protocol version 2. `RECONCILIATION.md` reconciles this server contract with `../android/spec.md` and answers that document's twelve open integration items — read it before changing anything the client depends on.

The implementation is Rust (edition 2024): axum + tokio for HTTP/WebSocket, bundled SQLite in WAL mode as the embedded database, ed25519-dalek for proof of possession, rustls for optional in-process TLS.

## Commands

```bash
cargo build                             # debug
cargo test                              # unit + integration (a couple of minutes)
cargo test --lib                        # unit only, fast, no sockets
cargo test --test acceptance_relay      # framing, replay, durability, restart/crash
cargo test --test acceptance_security   # identity, authorization, limits, settings
cargo test --test acceptance_ops        # listeners, TLS, retention, storage failure
cargo test --test acceptance_input      # bidirectional input, device roles, authorization
cargo test --test acceptance_relay criterion_9 -- --nocapture   # a single test
cargo clippy --all-targets              # the tree is warning-clean; keep it that way
cargo fmt

just up                                 # build + deploy (local mode, loopback only)
just up tailscale                        # onto a tailnet, real cert, no public exposure
just up proxy | just up tls              # other transport postures
just status | just logs | just token
just settings [NAME]                     # read settings from the container
just set NAME=VALUE                      # apply in place, no restart
just check                               # fmt + clippy -D warnings + test
```

The justfile is the deployment entry point; `deploy/docker-compose.yml`,
`deploy/docker-compose.tailscale.yml` and `deploy/kubernetes.yaml` are the underlying
manifests. `deploy/tailscale/README.md` explains why the Tailscale sidecar forwards raw
TCP rather than proxying HTTP, and why the two containers do not share a network
namespace — both are non-obvious and were chosen deliberately. `just up` remembers the mode and
ports in `deploy/.env` (also where the generated bootstrap secrets live — never commit it).

The binary has two subcommands besides running the relay: `healthcheck` (used by the
container health check, since the distroless image has no shell) and `settings get|set`,
which reaches the settings table directly for when the admin API is not yet reachable.

`RELAY_TEST_LOG=debug cargo test ... -- --nocapture` turns on server logs inside tests. It is a harness variable only and has no effect on the service.

Integration tests each start a real server on an ephemeral loopback port with its own temp database, so they run concurrently. Some deliberately exercise slow paths (challenge expiry, heartbeat timeout, retention sweeps).

## Where things live

| Path | Responsibility |
| --- | --- |
| `src/settings/defs.rs` | The setting registry — the single source of truth for every tunable |
| `src/settings/store.rs` | Seeding, atomic updates, audit, propagation |
| `src/relay/ring.rs` | The bounded replay buffer |
| `src/relay/terminal.rs` | Offsets, append, fan-out, close — the atomicity boundary |
| `src/relay/registry.rs` | Resident terminals, publisher leases, connection accounting |
| `src/relay/flush.rs` | The checkpoint task: batching, retry, backpressure |
| `src/relay/publisher.rs`, `mirror.rs` | The two WebSocket protocols |
| `src/db/` | Schema (`schema.sql`) and typed queries (`repo.rs`) |
| `src/http/` | Routing, request context, auth, resources, admin, ops |
| `src/server.rs` | Listener supervision, TLS, live rebind, graceful shutdown |

## Conventions this codebase holds to

- **Never add a behaviour knob outside `src/settings/defs.rs`.** No new env vars (the bootstrap set in `src/bootstrap.rs` is closed), no free-floating constants. Adding a setting there generates its name constant, metadata and self-checks automatically.
- **Never let the output hot path touch the database.** `TerminalHandle::append` must stay synchronous, lock-scoped and write-free.
- **Never persist or log terminal input.** It carries passwords. It stays out of the replay buffer, out of offsets, out of the database and out of logs; byte counts and sequence numbers only.
- **Re-check input authorization per frame, not per connection.** Three of the four conditions in spec §4.5 can change while a subscription is open, so the mirror takes a fresh settings snapshot for each input frame rather than reusing the loop's.
- **A version 1 peer must never observe version 2 behaviour.** New protocol fields are omitted, not defaulted, for v1 connections.
- **Comments explain why, not what.** Cite spec sections (`spec §7.2`) where behaviour is prescribed, since the reason is usually in the spec rather than the code.
- Tests assert spec-level behaviour and are named for the criterion they cover.

## Related repository

`../android/spec.md` specifies a separate Android client (C++17) that consumes this service. `RECONCILIATION.md` is the bridge between the two documents: it records how each conflict was resolved and answers the Android spec's §18 open items. Keep it current when you change anything on the client-facing surface.

Two conflicts were resolved by changing the *server*, and both are now implemented: terminal input (protocol version 2, spec §4.5 and §6.3), and the `client` device role that lets the phone hold its own credential instead of the identity's root key. `RECONCILIATION.md` §3 lists the edits the Android spec needed. The two largest are now done there — its §8.3 selects PTY-stream mode and its §7.2 speaks in byte offsets rather than revisions — so check that list against `../android/spec.md` before treating any item on it as outstanding.

## Architecture the spec mandates

Four concerns dominate the design and each is spread across several spec sections. Read them together before changing anything in these areas.

### Domain chain: identity → device → terminal

- An **identity** is a public key (Ed25519 minimum). Its ID is a deterministic fingerprint — `base64url(SHA-256(length_prefixed("terminal-relay-identity-v1", algorithm_id, canonical_public_key_bytes)))`, unpadded, 32-bit network-byte-order length prefixes. Re-registering the same canonical key MUST return the same identity. Adding a key algorithm MUST NOT change existing fingerprints.
- A **device** is a separately keyed principal owned by one identity, so no machine — publishing workstation or phone — ever holds the identity's root private key. Its `role` (`publisher`, `client`, or `both`) decides whether it may publish, mirror, or send input. Registration requires both an identity token and a signature from the proposed device key.
- A **terminal** is one output stream from one device, keyed by a device-local `local_ref` while open. Terminal IDs are never reopened or reused — a new session gets a new ID starting at offset 0. Its `accepts_input` flag is the publisher's opt-in to being written to.

### The offset model (§3.3, §6, §7)

Every correctness property hangs off three per-terminal 64-bit offsets:

```
earliest_offset  <=  durable_offset  <=  next_offset
retained_bytes = next_offset - earliest_offset  <=  1_500_000
```

- `next_offset` advances the instant bytes are accepted into memory; `durable_offset` advances only after a database transaction commits.
- Publisher output frames are accepted **only** when the frame's expected start offset equals the current `next_offset`. A mismatch must not append and must not mutate terminal state — reply `offset_mismatch` with authoritative offsets. This is what makes publisher retries idempotent.
- Never acknowledge past `durable_offset`. On restart, `next_offset` legitimately falls back to `durable_offset` and the publisher retransmits the memory-only suffix; once acknowledged as durable, offsets must never move backward.
- Subscribers requesting an offset below `earliest_offset` get a `gap` control message then replay from `earliest_offset`; above `next_offset` they get `offset_ahead` and the subscription fails.

### Memory-first buffering with batched checkpoints (§7.2)

The hot path MUST NOT perform a database write per WebSocket frame. Live and replay reads both come from a bounded in-memory ring buffer; dirty bytes from many frames (and, where the DB allows, many terminals) coalesce into infrequent transactions triggered by whichever comes first: `persistence.flush_interval` (default 5s), `persistence.flush_bytes` (default 262,144), terminal close, graceful shutdown, memory pressure, or an operator flush.

The **explicit exception** (§7.2, last paragraph): security-critical mutations — identity/device registration, device revocation, settings updates, signing-key state, consumption of single-use challenges and tickets — commit immediately when their API reports success. They never wait for the output flush interval.

On commit failure: keep dirty bytes, retry with backoff, apply publisher backpressure *before* memory eviction could lose data, fail readiness, and return `storage_unavailable`. Never issue a false durable ack.

### Database-backed runtime settings (§5.5, §8.1)

Every value that drives behavior is a typed row in the database, tunable at runtime through `/v1/admin/settings` without a process restart, and must survive restart. Code may hold initial defaults and hard safety bounds, and seeds them as rows on first init — after that the database is authoritative. Do not introduce free-floating constants or env vars for behavior.

- `PATCH` is optimistic-concurrency: it carries the revision the operator read; stale → `409`, invalid combination → `422` with nothing applied. Each change is one transaction, a new monotonic revision, and an audit-log entry with hashed (never raw) values.
- Each request, connection, and output batch uses **one immutable settings snapshot**, so a concurrent update can't produce internally inconsistent limits.
- Only values needed to locate/decrypt/authenticate to the settings database may come from env vars, CLI args, or mounted secret files (plus instance identity and emergency recovery mode). Nothing else.
- An invalid stored setting or unsupported schema version must fail readiness rather than fall back to a compiled default. "Operator must restart the container" does not satisfy runtime tunability.

## Wire-protocol details that are easy to get wrong

Four WebSocket subprotocols, negotiated explicitly, on two endpoints: `terminal-relay.publisher.v1`/`.v2` (`GET /v1/devices/{device_id}/relay`) and `terminal-relay.mirror.v1`/`.v2` (`GET /v1/terminals/{terminal_id}/mirror`). `.v2` adds terminal input. Auth completes during the HTTP upgrade — reject bad credentials *before* upgrading.

**The four binary frame layouts differ** (network byte order). Output is frame type `0x01`, input `0x02`; the direction that multiplexes many terminals over one connection is the one that carries the UUID:

| | Publisher → server (output) | Server → mirror (output) | Mirror → server (input) | Server → publisher (input) |
|---|---|---|---|---|
| byte 0 | `0x01` | `0x01` | `0x02` | `0x02` |
| bytes 1–16 | terminal UUID | — | — | terminal UUID |
| u64 field | bytes 17–24, *expected start offset* | bytes 1–8, *start offset* | bytes 1–8, *client sequence* | bytes 17–24, *relay sequence* |
| payload | bytes 25… | bytes 9… | bytes 9… | bytes 25… |

Other traps:

- Output **and input** bytes are **opaque**: no UTF-8 validation, no ANSI parsing, no newline normalization, no transformation of any kind, in either direction. Acceptance criteria 6 and 19 test exactly this.
- Input is not output: it never enters the replay buffer, never advances an offset, and is never persisted. A subscriber sees its own typing only via the remote terminal's echo.
- Zero-length output frames must never be sent to mirrors.
- Text frames are UTF-8 JSON control messages; binary frames are output. Malformed frames or unknown *required* message types → error message then close `1002`. Application errors use private close codes `4000`–`4999`.
- Replay capacity is **1,500,000 decimal bytes, not 1.5 MiB**, with a hard *schema maximum* at that value so no settings update can exceed the spec. Eviction may split a received frame. A single frame larger than the cap is accepted (subject to the negotiated frame-size limit), advances `next_offset` by its full length, and retains only its last 1,500,000 bytes.
- There must be no gap or reordering at the replay→live boundary. Slow subscribers have a bounded outbound queue and are closed with `slow_consumer`.
- Only one active publisher connection per device; a new authenticated one supersedes the old (closed with a `superseded` code).
- Publisher lifecycle (`terminal.open` / `terminal.resize` / `terminal.close`) goes over the relay WebSocket, not HTTP, so ordering against output is explicit. HTTP terminal resources are read-only for identity clients.

## Security and privacy rules that shape code structure

- **Authorization returns `404`, not `403`,** when an authenticated caller doesn't own a device or terminal — never reveal resource existence.
- Proof of possession everywhere: short-lived (≤5 min), single-use challenges, invalidated after the first verification attempt whether it succeeded or failed. The signed message is a versioned, unambiguous encoding binding service origin, challenge ID, challenge bytes, operation, key fingerprint, and expiry; prefer length-prefixed binary over JSON.
- Access tokens expire in ≤15 min. **Token material must never appear in a URL query string** — browser WebSocket clients use a secure HttpOnly SameSite cookie or a single-use, path-scoped ticket (≤60s) from `/v1/auth/websocket-tickets`.
- Private keys are never sent to or stored by the relay.
- Operator authentication for `/v1/admin/*` is a separate mechanism from identity and device auth.
- Device revocation blocks new auth immediately and kills existing tokens and WebSockets within 30 seconds.
- **Never log** terminal output payloads, tokens, tickets, challenges, signatures, raw public keys, or unredacted headers/bodies. **Never use** user-supplied labels or identity/device/terminal IDs as metric dimensions (high cardinality) — but every request and connection carries a correlation ID that belongs in structured logs.

## Container requirements (§8)

Non-root, minimal pinned base image, read-only root filesystem except mounted temp/data paths, one configurable HTTP port (default `8080`), logs to stdout/stderr, `SIGTERM` handling that stops new connections and drains writes before the shutdown deadline. Durable state lives on a persistent volume — embedded-database data directory defaults to `/var/lib/terminal-relay`. The container's ephemeral writable layer must never hold identities, devices, terminal metadata, committed offsets, challenges, revocations, settings, or replay checkpoints. `/healthz` must not depend on optional external services; `/readyz` succeeds only when auth, durable state, and relay traffic all work.

Single-instance deployment satisfies v1. Multi-instance requires shared durable state plus globally consistent per-terminal offset serialization, ticket consumption, revocation, and fan-out — sticky sessions alone are not sufficient.

## Versioning

Breaking HTTP changes need a new base path (`/v1` today); breaking WebSocket changes need a new subprotocol name. New optional JSON fields and ignorable control-message types are allowed within v1, but security-sensitive behavior must never depend on a peer ignoring something it doesn't understand.
