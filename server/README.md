# Terminal Mirror Relay

A containerised service that receives terminal output from registered devices and
streams it to authenticated clients over WebSockets, keeping a bounded replay window
per terminal so a new or reconnecting client can reconstruct recent output before
following the live stream.

`spec.md` is the normative specification. This implementation targets it clause by
clause; §11's twenty-three acceptance criteria are covered by the test suite (see
[Conformance](#conformance)).

The mirror is **bidirectional**: a subscriber sees the terminal's output, and an
authorized subscriber can type into it. Input arrived as protocol version 2 under the
authorization model in spec §4.5; version 1 remains served unchanged, and a version 1
peer never observes input. Command execution, file transfer and shell allocation
remain out of scope — the relay conveys bytes, it does not interpret them.

`RECONCILIATION.md` maps this contract onto the Android client specification and
answers its twelve open integration items.

## Contents

- [Quick start](#quick-start)
- [Build, test, lint](#build-test-lint)
- [Operator CLI](#operator-cli)
- [Bootstrap configuration](#bootstrap-configuration)
- [Runtime settings](#runtime-settings)
- [HTTP API](#http-api)
- [WebSocket protocols](#websocket-protocols)
- [Terminal input](#terminal-input)
- [Tailscale](#tailscale)
- [Buffering and durability](#buffering-and-durability)
- [Security model](#security-model)
- [Deployment](#deployment)
- [Observability](#observability)
- [Architecture](#architecture)
- [Conformance](#conformance)

## Quick start

Deployment is driven by [`just`](https://github.com/casey/just):

```bash
just up            # build and deploy; loopback only, no TLS ceremony
just status        # container, health and readiness
just token         # the operator credential
just logs
```

`just up` is re-runnable: it builds the current source, applies the settings for the
chosen mode, starts the container, waits for the health check, and prints where things
are. On the first run it generates the bootstrap secrets into `deploy/.env` (mode 0600)
and remembers the mode and ports there, so later deploys need no arguments.

### Deployment modes

`just up` takes the transport posture as its argument, because that is the one decision
the relay cannot make for you:

| Command | Posture |
| --- | --- |
| `just up` (`local`) | Published on `127.0.0.1` only, TLS not required. For development and single-machine use. |
| `just up tailscale` | Joins your tailnet. Reachable only by your own devices, with a real certificate. See [Tailscale](#tailscale). |
| `just up proxy` | A TLS-terminating reverse proxy sits in front and asserts `X-Forwarded-Proto`. TLS stays required. |
| `just up tls` | TLS terminated in-process from `deploy/tls/{cert,key}.pem`. Run `just tls-selfsigned` first for a local certificate. |

Only `local` relaxes `security.require_secure_transport`, and it publishes on loopback
only so nothing off the host can reach it. The command says which posture it deployed
and what that implies.

```bash
just port=9080 health_port=9081 up      # somewhere else
RELAY_PUBLIC_ORIGIN=https://relay.example just up proxy
just tls-selfsigned relay.local && just up tls
```

### Operating a running deployment

```bash
just settings                      # every setting, secrets redacted
just settings logging.level        # just one
just set logging.level=debug       # applied in place, no restart
just admin /v1/admin/settings      # authenticated admin API call
just metrics
just flush                         # force a checkpoint
just down                          # stop, keep the data volume
just destroy                       # stop and delete all durable state (confirms first)
```

`just set` needs no restart: a running instance observes the committed revision within
`settings.propagation_interval_ms` and rebinds its own listener if that is what
changed. It goes through the same validation, revision and audit path as the admin API,
so an invalid combination is rejected with nothing applied.

### Without just

The underlying pieces work on their own — `docker compose -f deploy/docker-compose.yml
up -d --build`, plus `terminal-relay settings set …` inside the container to configure
the transport posture. `deploy/kubernetes.yaml` is a single-instance example manifest.

## Build, test, lint

```bash
cargo build                    # debug build
cargo build --release          # optimised build

cargo test                     # everything: unit + integration
cargo test --lib               # unit tests only (fast, no sockets)
cargo test --test acceptance_relay      # relay: framing, replay, durability
cargo test --test acceptance_security   # identity, authorisation, limits, settings
cargo test --test acceptance_ops        # listeners, TLS, retention, storage failure
cargo test --test acceptance_input      # bidirectional input, roles, authorization

# One test, with server logs on stderr
RELAY_TEST_LOG=debug cargo test --test acceptance_relay \
  criterion_9_the_replay_window_never_exceeds_1_500_000_bytes -- --nocapture

cargo clippy --all-targets     # lints; the tree is warning-clean
cargo fmt                      # formatting
```

Integration tests start a real server on an ephemeral loopback port with its own
temporary database, so they can run concurrently. Several deliberately exercise slow
paths (challenge expiry, heartbeat timeouts, retention sweeps), so the suite takes a
couple of minutes.

`RELAY_TEST_LOG` is a test-harness variable only; it has no effect on the service.

## Bootstrap configuration

The environment may only supply what is needed to *locate, decrypt and authenticate
to* the settings database, plus instance identity and an emergency recovery mode
(spec §8.1). Nothing that drives relay, API, security-policy, retention, batching or
limit behaviour is configurable this way — those are database settings.

| Variable | Default | Purpose |
| --- | --- | --- |
| `RELAY_DATA_DIR` | `/var/lib/terminal-relay` | Directory for durable state. Must be a persistent volume. |
| `RELAY_DB_PATH` | `<data dir>/relay.db` | Database file location. |
| `RELAY_SECRET_KEY` | — | base64url of 32 bytes. Encrypts secrets stored in the database. |
| `RELAY_SECRET_KEY_FILE` | — | File containing the same, for secret managers that mount files. |
| `RELAY_OPERATOR_TOKEN` | — | Seeds `auth.operator_token_hash` on **first initialisation only**. |
| `RELAY_INSTANCE_ID` | hostname | Instance identity, reported by the admin API and logs. |
| `RELAY_RECOVERY_MODE` | `false` | Emergency mode: bypass the stored listen and TLS settings. |
| `RELAY_RECOVERY_LISTEN` | `127.0.0.1:8081` | Address used in recovery mode. |

If no bootstrap key is supplied, one is generated at `<data dir>/bootstrap.key` with
mode 0600 and a warning is logged. That is convenient for development but weaker than
a real secret manager, because the key then sits on the same volume as the database it
protects. Supply `RELAY_SECRET_KEY` or `RELAY_SECRET_KEY_FILE` in production.

If no operator token is supplied on first initialisation, one is generated and written
to `<data dir>/operator-token` (mode 0600); read it, then delete the file. It is never
logged.

### Recovery mode

If a committed settings revision makes the service unreachable — a bad listen address,
an unusable certificate path — start with `RELAY_RECOVERY_MODE=true`. That binds
`RELAY_RECOVERY_LISTEN` with the stored listen and TLS settings bypassed, so the
operator API is reachable to repair the revision. Then restart without the flag.

## Runtime settings

Every behaviour-driving value is a typed row in the `settings` table, tunable at
runtime and durable across restarts. There are 94 of them, grouped by dotted prefix:
`server.*`, `security.*`, `auth.*`, `features.*`, `ratelimit.*`, `limits.*`,
`terminal.*`, `persistence.*`, `mirror.*`, `websocket.*`, `settings.*`, `logging.*`,
`metrics.*`, `idempotency.*`.

The registry in `src/settings/defs.rs` is the single source of truth: one declaration
per setting generates its name constant, its metadata (type, default, bounds, allowed
values, secrecy, reload behaviour, description) and the startup self-check.

Read them, with secrets redacted:

```bash
curl -s http://localhost:8080/v1/admin/settings \
  -H "authorization: Bearer $OPERATOR_TOKEN" | jq '.revision, .settings[0]'
```

Update them atomically, quoting the revision you read:

```bash
curl -s -X PATCH http://localhost:8080/v1/admin/settings \
  -H "authorization: Bearer $OPERATOR_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"revision": 7, "settings": {"persistence.flush_interval_ms": 2000}}'
```

A stale revision answers `409`; an invalid value or an invalid *combination* answers
`422` with nothing applied. Every accepted change commits in one transaction, takes a
new monotonic revision, and is recorded in `settings_audit` with hashed old and new
values — raw values, and therefore secrets, never reach the audit log. Rejections are
recorded too. Read the recent history at `GET /v1/admin/settings/audit`.

### Operator CLI

Settings normally come from the admin API, but that needs a listener you can reach —
and a secure-by-default deployment has none until TLS is configured, which is itself a
setting. The binary therefore also speaks to the database directly:

```bash
terminal-relay settings get [NAME...]      # secrets are reported as configured, never shown
terminal-relay settings set NAME=VALUE...  # atomic, validated, audited
```

Values parse as JSON when possible and as a bare string otherwise, so both
`features.input_enabled=false` and `server.public_origin=https://relay.example` work.
Applying values that already match is a no-op, so a repeated deploy does not churn the
revision. It uses the same code path as `PATCH /v1/admin/settings`, so an invalid
combination is rejected the same way, and access is whoever can already read the
database file. `just settings` and `just set` wrap it in a one-shot container.

### Reload behaviour

Each setting declares how a change takes effect, reported in its metadata:

| Reload | Meaning |
| --- | --- |
| `immediate` | Applies to every operation begun after the revision is observed. |
| `connection_renegotiate` | Live connections keep the value negotiated at connect time; new connections get the new value. A **reduction** still applies at once, because reductions that bound resource use must not wait for a reconnect. |
| `listener_rebind` | Triggers an automated rebind with connection drain — no restart. |
| `logging_reload` | Reinitialises the logging subscriber in place. |
| `storage_reconfigure` | Re-applies SQLite pragmas to pooled connections as they are checked out. |

All healthy instances converge on a committed revision within
`settings.propagation_interval_ms`. Each request, connection and output batch captures
one immutable snapshot, so a concurrent update can never produce a mix of old and new
limits.

### Hard bounds

Code contains defaults and hard safety bounds, seeded into the database on first
initialisation; after that the database is authoritative. Some bounds exist so that no
update can violate the specification:

- `terminal.replay_capacity_bytes` has a schema maximum of **1,500,000** — decimal
  1.5 MB, not 1.5 MiB. It may be tuned downward only.
- `auth.challenge_ttl_seconds` ≤ 300, `auth.access_token_ttl_seconds` ≤ 900,
  `auth.websocket_ticket_ttl_seconds` ≤ 60.
- `idempotency.retention_seconds` ≥ 86,400.
- `persistence.flush_bytes` ≤ `limits.max_unacked_output_bytes`, so a publisher can
  always clear its own backlog.

An invalid stored value or an unsupported settings schema version fails readiness
rather than silently falling back to a compiled default.

### Naming note

The specification names three settings literally. Two match exactly:
`terminal.replay_capacity_bytes` and `persistence.flush_bytes`. The third,
`persistence.flush_interval`, is implemented as **`persistence.flush_interval_ms`** so
its unit is unambiguous — a bare `flush_interval` invites setting `5` and meaning
seconds. The schema minimum of 10 ms means a seconds-style value is rejected outright
rather than silently reducing the interval a thousandfold.

## HTTP API

Base path `/v1`, JSON bodies, lowercase canonical UUIDs, RFC 3339 UTC timestamps.
Errors always use:

```json
{ "error": { "code": "stable_machine_code", "message": "...", "request_id": "01K..." } }
```

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/v1/auth/challenges` | Create a proof-of-possession challenge. |
| `POST` | `/v1/identities` | Register a public key after proof of possession. |
| `POST` | `/v1/auth/tokens` | Exchange a completed challenge for an access token. |
| `POST` | `/v1/auth/websocket-tickets` | Mint a single-use, path-bound WebSocket ticket. |
| `POST` | `/v1/devices` | Register a device. |
| `GET` | `/v1/devices` | List owned, non-revoked devices (cursor paginated). |
| `GET` | `/v1/devices/{device_id}` | Fetch an owned device. |
| `DELETE` | `/v1/devices/{device_id}` | Revoke an owned device (idempotent). |
| `GET` | `/v1/terminals` | List owned terminals; filters `device_id`, `state`, `cursor`, `limit`. |
| `GET` | `/v1/terminals/{terminal_id}` | Terminal metadata plus offsets and `retained_bytes`. |
| `GET` | `/v1/devices/{device_id}/relay` | Publisher WebSocket upgrade. |
| `GET` | `/v1/terminals/{terminal_id}/mirror` | Mirror WebSocket upgrade. |
| `GET`/`PATCH` | `/v1/admin/settings` | Operator settings. |
| `GET` | `/v1/admin/settings/audit` | Recent settings audit entries. |
| `POST` | `/v1/admin/flush` | Force a checkpoint now. |
| `GET` | `/healthz` | Liveness; depends on nothing external. |
| `GET` | `/readyz` | Readiness: auth, durable state, relay acceptance, settings all healthy. |
| `GET` | `/metrics` | Prometheus text; operator-authenticated by default. |

Terminal resources are read-only over HTTP. Publishers manage terminal lifecycle over
their relay WebSocket so the ordering between lifecycle and output events is explicit.

Mutating requests honour `Idempotency-Key`. Replaying a key with the same body returns
the original response; a different body answers `409`.

### Registration walkthrough

```bash
# 1. Ask for a challenge. The response includes `signing_input`: the exact bytes to
#    sign, base64url encoded. The server always recomputes and verifies its own
#    derivation, so this is a convenience, not a trust anchor.
curl -s -X POST http://localhost:8080/v1/auth/challenges \
  -H 'content-type: application/json' \
  -d '{"operation":"register_identity","key":{"algorithm":"ed25519","public_key":"<base64url key>"}}'

# 2. Sign the decoded signing_input with the private key, then:
curl -s -X POST http://localhost:8080/v1/identities \
  -H 'content-type: application/json' \
  -d '{"challenge_id":"01K...","signature":"<base64url signature>"}'
```

The identity ID is a deterministic fingerprint of the key, so re-registering the same
key is idempotent and returns the same ID with `200` instead of `201`:

```text
identity_id = base64url_unpadded(SHA-256(
    lp("terminal-relay-identity-v1") || lp(algorithm_id) || lp(canonical_key_bytes)))
```

where `lp(x)` prefixes `x` with its length as a big-endian `u32`.

Device registration needs an identity token with `devices:write`, a `register_device`
challenge bound to that identity **and** to the proposed device key, and a signature
made by the device key. Device keys mean a device never holds the identity's root
private key.

## WebSocket protocols

Clients must negotiate an explicit subprotocol. Offer the highest you support; the
server selects it and echoes the choice in the handshake:

| Subprotocol | Endpoint | Adds |
| --- | --- | --- |
| `terminal-relay.publisher.v1` | `GET /v1/devices/{device_id}/relay` | output only |
| `terminal-relay.publisher.v2` | same | receives terminal input |
| `terminal-relay.mirror.v1` | `GET /v1/terminals/{terminal_id}/mirror` | output only |
| `terminal-relay.mirror.v2` | same | may send terminal input |

Authentication completes during the HTTP upgrade, via `Authorization: Bearer`, a
session cookie (`relay_session`), or a single-use ticket (`x-relay-ticket` header or
`relay_ticket` cookie). Missing, expired, path-mismatched or under-scoped credentials
are rejected **before** the upgrade. Text frames carry UTF-8 JSON control messages;
binary frames carry terminal output.

### Frame layouts

The two directions differ. A publisher frame carries the terminal UUID because one
connection multiplexes many terminals; a mirror frame does not, because the
subscription is already bound to one terminal. All integers are big-endian.

```text
publisher -> server (output)            server -> mirror (output)
byte  0     0x01                        byte 0     0x01
bytes 1-16  terminal UUID, raw          bytes 1-8  start offset, u64
bytes 17-24 expected start offset, u64  bytes 9..  opaque payload
bytes 25..  opaque payload

mirror -> server (input, v2)            server -> publisher (input, v2)
byte  0     0x02                        byte 0     0x02
bytes 1-8   client sequence, u64        bytes 1-16 terminal UUID, raw
bytes 9..   opaque payload              bytes 17-24 relay sequence, u64
                                        bytes 25.. opaque payload
```

Payload bytes are opaque: no UTF-8 validation, no ANSI parsing, no newline
normalisation, no transformation of any kind. Zero-length output frames are never sent
to subscribers.

### Publisher flow

1. Server sends `ready` with `connection_id`, the negotiated limits and the settings
   revision.
2. `terminal.open` → `terminal.opened` with `terminal_id`, `next_offset`,
   `durable_offset`, `earliest_offset` and `deduplicated`. Opening the same active
   `local_ref` again is idempotent.
3. Output frames are accepted **only** when the frame's expected start offset equals
   the server's current `next_offset`. A mismatch appends nothing, changes no state,
   and returns `offset_mismatch` with the authoritative offsets — which is what makes
   retries deterministic and prevents silent duplication after a reconnect.
4. `output.ack` is cumulative and is sent **only after** the batch containing those
   bytes has committed. Retain everything at or above the last acknowledged
   `durable_offset`.
5. `terminal.resize` and `terminal.close` manage lifecycle.

Only one publisher connection controls a device at a time; a newer authenticated
connection supersedes the older one, which closes with code 4002.

### Mirror flow

1. Server sends `ready`.
2. The client sends exactly one `subscribe`, optionally with `from_offset` (the offset
   of the next byte it wants). Omitting it requests the whole retained window.
3. Server replies `subscribed` with the requested and actual replay start, the offset
   trio, terminal state and size.
4. If the requested offset is below `earliest_offset`, a `gap` message precedes the
   replay. If it is above `next_offset`, the subscription fails with `offset_ahead`.
5. Replay is followed by live bytes with no gap and no reordering. `durable` messages
   raise the crash-safe watermark: advance a persistent resume cursor only on those.
6. `terminal.closed` arrives after every accepted byte has been delivered and
   committed.

### Close codes

Protocol violations use 1002. Application conditions use the private range:

| Code | Meaning |
| --- | --- |
| 1002 | Malformed frame or unknown required message type |
| 4001 | Unauthorized |
| 4002 | Superseded by a newer publisher connection |
| 4003 | Slow consumer: outbound queue bound exceeded |
| 4004 | Durable storage unavailable |
| 4005 | Requested offset ahead of `next_offset` |
| 4006 | Credential revoked |
| 4007 | Server shutting down |
| 4008 | Negotiated limit exceeded |
| 4009 | Heartbeat timeout |
| 4011 | Not found |
| 4012 | Rate limited |
| 4013 | Feature disabled |
| 4014 | Handshake timeout: no `subscribe` arrived |
| 4015 | Terminal closed |

New optional JSON fields may appear within version 1. A peer may mark a new control
message as ignorable with `"optional": true`; any other unknown type is treated as
required and fails the connection, so security-relevant behaviour never depends on a
silent skip.

## Terminal input

Input travels from a `mirror.v2` subscriber, through the relay, to the `publisher.v2`
connection that owns the terminal. The relay is a conduit: it does not interpret,
translate, echo, reorder or persist input.

### Authorization

Four conditions must hold, checked independently on **every frame** rather than trusted
from the handshake, because three of them can change while a subscription is open:

1. The subscriber's token carries `terminals:input`, and it owns the terminal — as the
   identity itself, or as a `client`/`both` device of that identity.
2. The publishing device opted in, opening the terminal with `accepts_input: true`.
   A device that never opts in can never be written to, whatever a token claims.
3. `features.input_enabled` is true. It is a security control, so turning it off stops
   input on connections that are already open.
4. A `publisher.v2` connection is currently attached for the device. Input is never
   queued for a disconnected publisher.

`subscribed` reports `accepts_input` and `input_available`; present a read-only state
when `input_available` is false rather than dropping keystrokes silently.

### Sequencing and acknowledgement

Number input frames from 1 per connection, increasing by exactly one. The relay
acknowledges only after handing the frame to the publisher:

```json
{ "type": "input.ack", "accepted_through": 42, "relay_sequence": 913 }
```

Input is **at-most-once**. The relay never replays it, and a client must not resend
unacknowledged input after reconnecting — a half-delivered keystroke sequence replayed
into a live shell is worse than a lost one. A refused frame does not consume its
sequence, so transient refusals (`input_undeliverable`, `input_backpressure`) may be
retried with the same number.

### Refusal codes

| Code | Meaning | Transient |
| --- | --- | --- |
| `input_not_accepted` | The terminal did not opt in | no |
| `input_forbidden` | Missing scope, or not an authorized writer | no |
| `input_disabled` | An operator disabled input | no |
| `input_undeliverable` | No version 2 publisher connected | yes |
| `input_backpressure` | The publisher's queue is full | yes |
| `input_sequence_mismatch` | Not the expected next sequence | no |
| `limit_exceeded` | Larger than `limits.max_input_frame_bytes` | no |
| `rate_limited` | Per-subscriber input rate exceeded | yes |

### Resize

A subscriber with input authority may send
`{"type":"terminal.resize_request","cols":100,"rows":30}`. The relay forwards it; the
publisher remains the sole authority over the dimensions and reports the outcome as an
ordinary `terminal.resize` that every subscriber sees. Operators can disable this with
`features.client_resize_enabled` independently of input.

### What input never touches

Input is not output. It never enters the replay buffer, never advances an offset, is
never written to the database, and is never logged — only byte counts and sequence
numbers are recorded. What a subscriber sees of its own typing is whatever the remote
terminal echoes back as output, so a password prompt with echo disabled correctly
shows nothing.

## Device roles

A device is a separately keyed principal owned by one identity, with a role:

| Role | Publishes | Mirrors the owner's terminals | Sends input |
| --- | --- | --- | --- |
| `publisher` (default) | yes | no | no |
| `client` | no | yes | yes |
| `both` | yes | yes | yes |

The `client` role is what lets a phone hold its own revocable credential instead of the
identity's root private key. Register it with `"role": "client"` on `POST /v1/devices`.
Settings validation refuses any scope configuration that would give a publisher input
or identity-level authority, or a client the ability to publish.

Because `POST /v1/devices` needs an identity token, a phone is paired rather than
self-registered: it shows its public key, and a machine holding the identity key
registers it. Both parties act, which is what makes the pairing meaningful.
`RECONCILIATION.md` §2.2 describes the flow.


## Tailscale

`just up tailscale` puts the relay on your tailnet. Only your own devices can reach
it, nothing is exposed to the internet, no ports are forwarded, and the certificate is
a real one that Tailscale issues and renews for the node's MagicDNS name.

That last point matters more than it looks. The Android client validates certificates
in release builds, so a self-signed certificate is not an option and a public
deployment would otherwise need a domain, a real certificate and an open port. This
gives you a valid `https://` endpoint with none of that.

```bash
just up tailscale
# ==> selecting a free subnet — 172.16.250.0/29
# ==> starting tailscale sidecar
#     authorise here:  https://login.tailscale.com/a/...
# ==> tailnet name: hypeterm-relay.example.ts.net
```

Point the mobile app at `https://hypeterm-relay.example.ts.net`. Both ends must
be on the same tailnet: the server joins once at deploy time, and the app joins when
you sign it in. That is the irreducible part — a private network needs both ends to be
members — but it is one login each, not a Tailscale install and a config file.

### One-time tailnet prerequisites

Tailscale will only issue a certificate once **MagicDNS** and **HTTPS Certificates**
are both enabled for the tailnet, under
[admin → DNS](https://login.tailscale.com/admin/dns). `just up tailscale` checks this
after the node joins and stops with instructions rather than letting TLS fail later
and opaquely.

Enabling HTTPS publishes this node's name on a public certificate-transparency ledger,
so `hypeterm-relay.example.ts.net` becomes publicly known as a *name*. It stays
unreachable to anyone outside the tailnet; only its existence is disclosed.

### Authenticating the server

Without `TS_AUTHKEY` the sidecar prints a login link, which `just up tailscale`
surfaces. The recipe passes `--timeout=30m` so the link stays valid long enough to go
and click it — otherwise tailscaled gives up after a couple of minutes and restarts
with a new one. If it is replaced anyway, the current link is printed again.

For unattended deploys, generate an auth key at
`https://login.tailscale.com/admin/settings/keys` and put it in `deploy/.env`:

```
TS_AUTHKEY=tskey-auth-...
```

Node identity lives on a volume, so restarts rejoin as the same node rather than
registering a new one each time.

### What expires, and what renews itself

Three things have a lifetime. Two look after themselves:

- **The TLS certificate** (90 days, Let's Encrypt) renews automatically, because
  tailscaled is the thing terminating TLS. This is a direct consequence of the design
  choice above: had the relay instead read certificate files produced by
  `tailscale cert`, renewal would have been your job, since the daemon has no idea
  where such files were put or how to reload them.
- **The relay's token signing keys** rotate on `auth.signing_key_rotation_seconds`
  (30 days) with an overlap window, so tokens minted just before a rotation keep
  verifying.

One does not:

- **The tailnet node key expires after 180 days by default.** When it does, the node
  drops off the tailnet and has to be re-authenticated by hand — the mobile app simply
  stops being able to reach the server. Disable it once, under
  [admin → Machines](https://login.tailscale.com/admin/machines) → the node's menu →
  *Disable key expiry*; Tailscale recommends exactly this for trusted always-on
  servers. `just status` reports the expiry date and days remaining, and warns inside
  30 days.

The `TS_AUTHKEY`, if you used one, expires too (90 days maximum), but only matters for
the initial registration: `TS_AUTH_ONCE=true` plus node identity on the state volume
means restarts rejoin without it. You would need a fresh key only after destroying that
volume — which `just destroy` does.

### How the traffic flows

```
mobile app ──WireGuard──▶ tailscale sidecar ──plain TCP──▶ relay
                          terminates TLS                   172.x.x.3:8080
```

Tailscale terminates TLS and forwards **raw TCP**, rather than acting as an HTTP
reverse proxy. The relay is almost entirely WebSockets, and Serve's HTTP proxy path
has a history of upgrade regressions and strips query parameters from upgrade
requests; after TLS termination a TCP forward is just a byte stream, so none of that
applies.

The two containers are independent, on a small dedicated bridge network, rather than
sharing a network namespace. `network_mode: service:tailscale` is the more common
recipe, but it makes the relay's networking hostage to the sidecar: a restarting
sidecar gets a new namespace and leaves the relay orphaned with a dead network while
still reporting healthy. A crash restart is not orchestrated by Compose, so
`depends_on` cannot rescue it. With separate networks the sidecar can be recreated
freely and the relay never notices.

### What this trades away

A raw TCP forward carries no client address, so the relay sees every tailnet
connection as coming from the sidecar. Per-**source** rate limits therefore collapse
into one bucket. The per-**principal** limits, keyed by the authenticated token, are
unaffected, and on a tailnet those are the meaningful ones.

Transport security is likewise established by address rather than by a forwarded
header: `just up tailscale` sets
`security.tls_terminated_by_networks` to the dedicated subnet, so connections from it
count as TLS-terminated. That subnet contains nothing but the relay and the sidecar,
the relay's API is deliberately **not** published to the host, and the development
loopback exemption stays off.

Because the API is not published, `just admin`, `just metrics` and `just flush` make
their request from inside the sidecar. `just settings`, `just set` and `just status`
work as usual.

## Buffering and durability

Each terminal carries three 64-bit offsets:

```text
earliest_offset  <=  durable_offset  <=  next_offset
retained_bytes = next_offset - earliest_offset  <=  1,500,000
```

- `next_offset` advances the moment bytes are accepted into memory.
- `durable_offset` advances only after a database transaction commits.
- `earliest_offset` is the first byte still available for replay.

Accepting and relaying an output frame performs **no** database write. Live reads and
replay both come from a bounded in-memory ring buffer; dirty output from many frames
and many terminals coalesces into infrequent transactions, triggered by whichever
comes first: `persistence.flush_interval_ms` since the oldest dirty byte,
`persistence.flush_bytes`, a terminal closing, graceful shutdown, memory pressure, or
`POST /v1/admin/flush`.

Payload is stored as append-only chunks, so a checkpoint appends each byte once
instead of rewriting the retained suffix. Eviction is a coalesced range delete plus at
most one straddling-chunk trim.

When the unacknowledged window is full the publisher is held back until a checkpoint
completes, rather than evicting bytes that were never persisted. If storage keeps
failing, dirty bytes stay in memory, readiness fails, and publishers receive
`storage_unavailable` — a false durable acknowledgement is never issued.

After a restart, buffers rebuild from the latest committed state and `next_offset`
resumes at `durable_offset`. Bytes relayed live above that point are naturally
retransmitted by the publisher when it reconnects, because `terminal.opened` tells it
the authoritative offset. Once acknowledged, offsets never move backward.

A frame larger than the whole replay window is accepted, as the specification
requires: offsets advance by its full length and only its newest 1,500,000 bytes are
retained.

## Security model

### Transport security

`security.require_secure_transport` defaults to true. A request qualifies as secure if
TLS was terminated in-process (`server.tls_enabled`) or a **trusted** proxy asserts it
via `security.forwarded_proto_header`. Forwarded headers are honoured only when the
immediate peer falls inside `security.trusted_proxy_networks`.

`security.allow_insecure_loopback` (default true) exempts loopback peers so local
development works without certificates. Disable it in production. Plain HTTP is
otherwise refused with `403 insecure_transport`; the one documented exception is the
isolated health listener (`server.health_listen_address`), which serves only
`/healthz` and `/readyz`.

Private keys are never sent to, or stored by, the relay.

### Proof of possession

Challenges are single-use and short-lived (≤5 minutes), and are invalidated by the
first verification attempt whether it succeeds or fails. Each is rate limited by
source and by key fingerprint. The signed message is a versioned, length-prefixed
binary encoding that binds the service origin, challenge ID, challenge bytes, intended
operation, key fingerprint, owner identity, device key fingerprint and expiry — every
field always present, so no boundary is ambiguous.

### Tokens and scopes

Access tokens are HMAC-signed, expire in ≤15 minutes, and carry subject, principal
kind, owning identity, issuer, audience, issue and expiry times, a unique token ID and
granted scopes. Signing keys live encrypted in the database under bootstrap key
material and rotate with an overlap window.

Identity tokens may hold `devices:read`, `devices:write`, `terminals:read`,
`terminals:mirror`. Device tokens may hold only `terminals:write`, `terminals:publish`
— the settings validator rejects any configuration that would give a device
identity-level authority.

**Token material must never appear in a URL.** Requests carrying `access_token`,
`token`, `ticket` or `bearer` in the query string are rejected outright, even when the
value is valid. Browser clients should use a secure `HttpOnly` `SameSite` cookie or a
single-use WebSocket ticket.

### Authorization

Terminal streams are private to their owning identity. An identity may list and mirror
only its own devices and terminals; a device may publish only to its own terminals.
Ownership failures answer **404, not 403**, so resource existence is never revealed.

Revoking a device blocks new authentication immediately and terminates existing tokens
and WebSockets promptly — live connections also re-check durable revocation state every
`security.revocation_recheck_seconds` (≤30), which bounds enforcement even across
instances.

### Privacy

Terminal output payloads, tokens, tickets, challenges, signatures, raw public keys and
unredacted headers are never logged. User-supplied labels and identity, device or
terminal IDs are never metric dimensions. Terminal contents are sensitive: protect
backups, volumes and endpoints accordingly, and prefer encryption at rest.

## Deployment

The image satisfies the specification's container requirements:

- runs as non-root (UID 65532)
- minimal pinned base (`gcr.io/distroless/cc-debian12:nonroot`)
- one configurable HTTP port, default 8080
- logs to stdout/stderr as JSON
- handles `SIGTERM`: stops accepting, notifies peers, commits dirty output, exits
  inside `server.shutdown_deadline_seconds`
- a `HEALTHCHECK` using the binary's own probe, so no shell or curl is needed
- a read-only root filesystem; only `/var/lib/terminal-relay` and `/tmp` are writable

Durable state — identities, devices, terminal metadata, committed offsets, challenges,
revocations, settings and replay checkpoints — lives in SQLite (WAL) under
`/var/lib/terminal-relay`. That path must be a persistent volume; the container's
ephemeral writable layer must never hold it.

Set `stop_grace_period` (compose) or `terminationGracePeriodSeconds` (Kubernetes)
above `server.shutdown_deadline_seconds` so the drain finishes before the runtime
kills the container. Examples: `deploy/docker-compose.yml`, `deploy/kubernetes.yaml`.

Single-instance deployment is sufficient for version 1. A multi-instance deployment
additionally needs shared durable state with globally consistent per-terminal offset
serialisation, ticket consumption, revocation and subscriber fan-out; sticky sessions
alone are not a correctness guarantee.

Memory is bounded by roughly
`active terminals × terminal.replay_capacity_bytes + persistence.memory_pressure_dirty_bytes`,
plus each subscriber's `mirror.subscriber_queue_bytes`.

## Observability

Every HTTP request and WebSocket connection gets a correlation ID, returned as
`x-request-id`, echoed in error bodies, and present on every log line for that
request. Logs are structured, with event type, correlation ID, principal, device or
terminal ID where applicable, result, latency and byte counts.

`GET /metrics` exposes Prometheus text covering connections, terminals by state,
accepted and delivered bytes, replay and dirty bytes, durable-offset lag, checkpoint
counts, batch size and age histograms, evictions, offset mismatches, slow-consumer
disconnects, authentication failures, storage errors, the active and committed
settings revisions, propagation lag, and request latency.

## Architecture

| Path | Responsibility |
| --- | --- |
| `src/settings/defs.rs` | The setting registry: one declaration per setting generates names, metadata and self-checks. |
| `src/settings/store.rs` | Seeding, atomic updates, audit, propagation. |
| `src/relay/ring.rs` | The bounded replay buffer. |
| `src/relay/terminal.rs` | Offsets, append, subscriber fan-out, close — the atomicity boundary. |
| `src/relay/registry.rs` | Resident terminals, publisher leases, connection accounting. |
| `src/relay/flush.rs` | The checkpoint task: batching, retry, backpressure. |
| `src/relay/publisher.rs`, `mirror.rs` | The two WebSocket protocols. |
| `src/relay/frames.rs`, `messages.rs` | Wire formats. |
| `src/db/` | Schema and typed queries. |
| `src/http/` | Routing, request context, auth, resources, admin, ops. |
| `src/server.rs` | Listener supervision, TLS, live rebind, graceful shutdown. |

## Conformance

The specification's eighteen acceptance criteria map onto the test suite:

| Criterion | Test |
| --- | --- |
| 1. Registration requires a valid, unexpired, single-use challenge | `criterion_1_registration_requires_a_valid_unexpired_single_use_challenge`, `criterion_1_expired_challenges_are_refused` |
| 2. Re-registering a key returns the same identity | `criterion_2_reregistering_the_same_key_returns_the_same_identity` |
| 3. Multiple independently keyed devices, registered and revoked | `criterion_3_an_identity_manages_multiple_independently_keyed_devices` |
| 4. Cross-device and cross-identity isolation | `criterion_4_isolation_between_devices_and_identities` |
| 5. Zero, one or many concurrent terminals | `criterion_5_a_device_advertises_zero_one_or_many_terminals` |
| 6. Arbitrary binary output relayed unmodified and in order | `criterion_6_arbitrary_binary_output_is_relayed_unmodified_and_in_order` |
| 7. Replay then live, without duplication, loss or reordering | `criterion_7_replay_is_followed_by_live_bytes_with_no_seam` |
| 8. Resume from a processed offset; explicit gap when evicted | `criterion_8_a_subscriber_resumes_from_a_processed_offset`, `criterion_8_an_evicted_offset_produces_an_explicit_gap` |
| 9. Never more than 1,500,000 retained bytes; newest suffix | `criterion_9_the_replay_window_never_exceeds_1_500_000_bytes` |
| 10. Bounded in-memory ring; frames coalesced into far fewer transactions | `criterion_10_many_frames_coalesce_into_far_fewer_transactions` |
| 11. Acknowledgement follows commit; survives restart | `criterion_11_acknowledgements_follow_commits_and_survive_restart` |
| 12. Crash rolls back to `durable_offset`; retransmission does not duplicate | `criterion_12_a_crash_rolls_back_to_durable_offset_without_duplication` |
| 13. Retries do not duplicate; mismatches do not mutate | `criterion_13_retries_do_not_duplicate_and_mismatches_do_not_mutate` |
| 14. Slow consumers, oversized frames, excessive terminals, rate limits | the four `criterion_14_*` tests |
| 15. Revocation blocks new access and terminates existing | `criterion_15_revocation_blocks_new_access_and_terminates_existing` |
| 16. Every behaviour value is a typed setting; valid updates apply and persist; invalid updates are atomically rejected | the four `criterion_16_*` tests |
| 17. Consistent snapshots; instances converge | `criterion_17_snapshots_are_internally_consistent`, `criterion_17_instances_converge_on_the_committed_revision` |
| 18. Container runs non-root, reports health, stores state on a volume, shuts down gracefully after flushing | `criterion_18_graceful_shutdown_flushes_dirty_output`, `durable_state_lives_in_the_configured_data_directory`, plus the image properties asserted in the `Dockerfile` and verified by `docker inspect`/`docker top` |
| 19. Input reaches the device intact, in order, exactly once; its echo returns as output | `criterion_19_input_reaches_the_publisher_and_its_echo_returns_as_output` |
| 20. Input refused with a distinct code and no delivery when any authorization condition fails | the five `criterion_20_*` tests |
| 21. Input never enters the replay buffer, durable storage, offsets or logs | `criterion_21_input_never_enters_the_replay_buffer_or_durable_state` |
| 22. Version 1 peers unaffected; a v1 publisher cannot claim to accept input | `criterion_22_version_1_peers_are_unaffected` |
| 23. A client device works without the root key, and revoking it ends that access | `criterion_23_a_client_device_works_without_the_root_key` |

Beyond those, the suite covers TLS termination, live listener rebind, the isolated
health listener, transport-security enforcement, retention and quota sweeps, commit
failure withholding acknowledgement, heartbeat and handshake timeouts, publisher
supersede and reconnect grace, multi-subscriber fan-out, per-terminal ordering,
feature switches, idempotency keys, cursor pagination, input sequencing and rate
limits, client-requested resize, scope-escalation guards, and the error envelope on
every rejection path — 118 tests in total.
