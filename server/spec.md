# Terminal Mirror Relay Specification

## 1. Purpose

The Terminal Mirror Relay is a containerized service that receives terminal output from registered devices and streams it to authenticated clients over WebSockets. It also keeps a bounded replay window for each terminal so that a newly connected or reconnecting client can reconstruct recent output before following the live stream.

This specification uses the terms **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** as defined by RFC 2119 and RFC 8174.

## 2. Scope

The service MUST provide:

- self-service API identity registration based on possession of a public key;
- one identity owning zero or more registered devices, each acting as a publisher, an interactive client, or both;
- one publisher device advertising zero or more terminal sessions;
- authenticated publication of terminal output by a device;
- authenticated WebSocket subscription to replayed and live terminal output;
- authenticated delivery of terminal input from a subscriber back to the publishing device;
- a per-terminal replay window of no more than 1,500,000 output bytes;
- resumable streaming using monotonically increasing byte offsets;
- memory-first terminal buffering with infrequent, batched database checkpoints;
- database-backed runtime settings for every value that controls service behavior;
- container deployment with durable state stored outside the container's writable layer; and
- health and readiness endpoints suitable for container orchestration.

Protocol version 1 was an output mirror only. **Protocol version 2 adds terminal input**, under the explicit authorization model in §4.5 and the new subprotocol names in §6, as version 1 of this document required of any such addition. Version 1 subprotocols remain supported unchanged, and a version 1 peer never observes input.

Executing commands directly, file transfer, port forwarding, shell allocation by the relay, and collaborative control arbitration between competing writers remain out of scope. The relay conveys input bytes; it does not interpret them, allocate terminals, or decide which of several authorized writers should win.

## 3. Domain model

### 3.1 Identity

An identity represents an API user and is defined by a supported public key. The public key is the identity's root credential; no username or email address is required.

The canonical identity ID MUST be a stable fingerprint:

```text
identity_id = base64url(SHA-256(length_prefixed(
    "terminal-relay-identity-v1",
    algorithm_id,
    canonical_public_key_bytes
)))
```

The encoding MUST omit Base64 padding. Length prefixes are unsigned 32-bit network-byte-order values. The service MUST canonicalize and validate a key before calculating its fingerprint. At minimum, Ed25519 keys MUST be supported. Additional algorithms MAY be added with an explicit algorithm identifier and must not change existing fingerprints.

Registering the same canonical public key more than once MUST be idempotent and return the same identity.

### 3.2 Device

A device is a separately keyed principal owned by exactly one identity. It has:

- a server-generated UUID `device_id`;
- an owner `identity_id`;
- its own public key and key fingerprint;
- an owner-scoped, non-unique display name;
- a role;
- creation and last-seen timestamps; and
- a revocation timestamp, when revoked.

The role determines what the device may do and MUST be one of:

| Role | May publish terminals | May mirror the owner's terminals | May send input |
|---|---|---|---|
| `publisher` | yes | no | no |
| `client` | no | yes | yes, subject to §4.5 |
| `both` | yes | yes | yes, subject to §4.5 |

`publisher` is the default when a registration does not state a role, so a version 1
client that omits the field keeps its existing meaning.

Device keys allow both publishing machines and interactive clients to connect without copying the owner's root private key onto every machine, which matters most for mobile clients whose credential storage the owner does not control. A device MUST prove possession of its private key during registration, and the owning identity MUST authorize the registration. Revoking a device MUST immediately prevent new authentication and relay connections for it, whatever its role.

### 3.3 Terminal

A terminal is one output stream published by one device. It has:

- a server-generated UUID `terminal_id`;
- its owning `device_id` and, transitively, `identity_id`;
- a device-supplied label;
- an opaque device-local reference, unique among that device's active terminals;
- lifecycle state: `open` or `closed`;
- an `accepts_input` flag, declared by the publishing device, defaulting to false;
- optional terminal metadata such as columns, rows, terminal type, and host-local process label;
- creation, last-activity, and close timestamps;
- a monotonically increasing 64-bit `next_offset`; and
- a monotonically increasing 64-bit `durable_offset`, never greater than `next_offset`; and
- a bounded replay buffer.

Terminal metadata MUST NOT include environment variables, command arguments, working directories, or other potentially sensitive process details unless the publisher deliberately supplies them and the implementation documents the exposure.

Opening a terminal using the same device-local reference while it is already open MUST be idempotent. Once closed, a terminal ID MUST NOT be reopened or reused; a new process/session receives a new terminal ID and starts at offset zero.

## 4. Security model

### 4.1 Transport security

All non-development traffic MUST use HTTPS and secure WebSockets (`wss://`). The service MAY terminate TLS itself or run behind a trusted TLS-terminating reverse proxy. Plain HTTP MUST be disabled in production except for an explicitly isolated health endpoint.

Private keys MUST never be sent to or stored by the relay.

### 4.2 Proof of possession

Registration and authentication MUST use short-lived, single-use challenges:

1. The caller requests a challenge and supplies the key algorithm and public key.
2. The service returns a cryptographically random `challenge_id`, at least 32 random challenge bytes, an expiry time no more than five minutes in the future, and the exact signature context.
3. The caller signs the context plus challenge using the corresponding private key.
4. The service verifies the signature, expiry, intended operation, and one-time use.

The signed message MUST be an unambiguous, versioned encoding that includes the service origin, challenge ID, challenge bytes, intended operation, public-key fingerprint, and expiry. JSON may be used only with a specified canonicalization scheme; a length-prefixed binary encoding is preferred.

Challenges MUST be rate-limited by source and key fingerprint. A challenge MUST be invalidated after its first verification attempt, whether verification succeeds or fails.

### 4.3 Sessions

Successful identity or device authentication returns a short-lived bearer access token. Tokens MUST expire in no more than 15 minutes and MUST include or reference:

- the authenticated identity or device ID;
- the token audience and issuer;
- issue and expiry times;
- a unique token ID; and
- granted scopes.

Identity tokens MAY receive `devices:read`, `devices:write`, `terminals:read`, `terminals:mirror`, `terminals:input`, and `terminals:create` scopes. Device tokens MAY receive only the scopes their role permits: a `publisher` device receives only scopes needed to manage and publish that device's terminals, and never an identity-management scope; a `client` device receives only `terminals:read`, `terminals:mirror`, and, when input is enabled, `terminals:input`. A `client` or `both` device MAY additionally receive `terminals:create` and `devices:read`. Neither is granted by default: an operator MUST add them explicitly. `terminals:create` is equal in gravity to `terminals:input` — a credential holding both is shell-equivalent on the publishing machine — and MUST NOT be treated as a lesser, read-like capability. Token material MUST NOT appear in URL query strings. Browser clients SHOULD authenticate a WebSocket with a secure, HttpOnly, SameSite cookie or a short-lived single-purpose WebSocket ticket.

### 4.4 Authorization

Terminal streams are private to their owning identity. An identity, and any `client` or `both` device it owns, MAY list and mirror only that identity's devices and terminals. A `publisher` or `both` device MAY create, update, close, and publish only terminals belonging to that device. Cross-identity sharing is out of scope.

The API MUST return `404 Not Found`, rather than revealing resource existence with `403 Forbidden`, when an authenticated caller does not own a requested device or terminal. Insufficient scope on an owned resource MUST return `403 Forbidden`, because that reveals nothing the caller did not already know.

A `client` or `both` device MAY also list its owning identity's devices, which is how it learns on which machine it could ask for a terminal, and MAY ask a `publisher` or `both` device of that identity to open one, subject to §4.6.

### 4.5 Input authorization

Terminal input is a distinct authority from reading a terminal, and version 1 of this
document required an explicit authorization model before it could be added. Every
input byte the relay delivers MUST satisfy **all** of the following, checked
independently:

1. **The subscriber is authorized.** Its token carries `terminals:input`, and it is
   the identity that owns the terminal or a `client`/`both` device of that identity.
2. **The publishing device opted in.** The terminal was opened with
   `accepts_input: true`. A device that never opts in can never be written to,
   regardless of what any token claims. This is what keeps a compromised or
   over-scoped reader from reaching a machine that only meant to broadcast.
3. **The deployment allows it.** The operator setting `features.input_enabled` is
   true. Setting it false MUST stop input immediately on existing connections, since
   it is a security control rather than a negotiated limit.
4. **A publisher is connected.** Input MUST NOT be queued, buffered, or replayed for a
   device that is not currently connected; the subscriber is told instead.

Input MUST NOT be written to the replay buffer, to durable storage, or to logs. Only
byte counts and sequence numbers may be recorded. Input is not part of the output
stream: what a subscriber sees of its own typing is whatever the remote terminal
echoes back as ordinary output, so a terminal with echo disabled — a password prompt —
correctly shows nothing.

The relay does not arbitrate between multiple authorized writers. When several
subscribers send input to one terminal, the relay delivers each frame whole and in
arrival order, and never interleaves bytes within a frame. Deployments that need a
single writer should authorize a single client.

### 4.6 Terminal creation authorization

A terminal may come into existence only through a publishing device's `terminal.open` (§6.1). A subscriber may *ask* a device it owns to open one. Every such request MUST satisfy **all** of the following, checked independently:

1. **The caller is authorized.** Its token carries `terminals:create`, and it is the identity that owns the target device or a `client`/`both` device of that identity. A device that is not owned, does not exist, or has been revoked MUST answer `404` (§4.4). The target device's role MUST permit publishing.
2. **The publishing device opted in.** It asserted `publisher.capabilities` with `terminal_open_requests: true` on its current relay connection, and it MUST re-check its own local policy before creating any process. A device that never opts in can never be made to spawn a process, whatever any token claims and whatever the relay sends. The server MUST NOT send `terminal.open_request` to a device that has not asserted the capability, and MUST NOT infer the capability from the mere presence of a connection.
3. **The deployment allows it.** The operator setting `features.terminal_create_enabled` is true. It defaults to false, so upgrading a server never grants the capability; setting it false MUST stop new requests immediately on existing connections, since it is a security control rather than a negotiated limit.
4. **A capable publisher is connected.** A request MUST NOT be queued, buffered, or replayed for a device that is not currently connected; the caller is told instead.
5. **The operator granted the scope.** `terminals:create` is not among the default token scopes for any principal kind.

The request MUST NOT carry a command, argument vector, environment, working directory, `TERM`, `accepts_input`, `local_ref`, or `process_label`, and a server MUST reject a request containing any of them rather than ignoring it. The publishing device alone determines the program, its environment, its working directory, and whether the resulting terminal accepts input.

A server MUST NOT relay a publisher's free-text decline detail to the requesting client.

## 5. HTTP API

The API base path is `/v1`. Request and response bodies use `application/json`. UUIDs use the lowercase canonical textual representation. Timestamps use UTC RFC 3339 with fractional seconds allowed.

Errors MUST use this shape:

```json
{
  "error": {
    "code": "stable_machine_code",
    "message": "Human-readable description",
    "request_id": "01K..."
  }
}
```

The server MUST enforce request-size limits, validate all fields, ignore no unknown security-sensitive fields, and return `429 Too Many Requests` with `Retry-After` when rate limits apply.

### 5.1 Identity registration and authentication

#### `POST /v1/auth/challenges`

Creates a proof-of-possession challenge.

Request:

```json
{
  "operation": "register_identity",
  "key": {
    "algorithm": "ed25519",
    "public_key": "<base64url canonical key bytes>"
  }
}
```

`operation` is one of `register_identity`, `authenticate_identity`, `register_device`, or `authenticate_device`. A device-registration challenge MUST additionally bind the intended owner identity and proposed device-key fingerprint.

Response: `201 Created` with `challenge_id`, `challenge`, `signature_context`, and `expires_at`.

#### `POST /v1/identities`

Registers a key after proof of possession.

Request:

```json
{
  "challenge_id": "01K...",
  "signature": "<base64url signature>"
}
```

Response: `201 Created`, or `200 OK` for an existing identity, with `identity_id` and `created_at`.

#### `POST /v1/auth/tokens`

Exchanges a completed identity or device authentication challenge and signature for an access token. Response: `200 OK` with `access_token`, `token_type: "Bearer"`, `expires_in`, and scopes.

#### `POST /v1/auth/websocket-tickets`

Creates a single-use ticket, valid for no more than 60 seconds, for one specific mirror or device-relay WebSocket path. Using or attempting to use the ticket consumes it. Response: `201 Created` with `ticket` and `expires_at`.

### 5.2 Devices

#### `POST /v1/devices`

Registers a device. Requires an identity token with `devices:write`, a `register_device` challenge bound to the authenticated identity, and a signature made by the proposed device key.

Request fields are `name`, `key`, `challenge_id`, `device_signature`, and an optional `role` of `publisher`, `client`, or `both`, defaulting to `publisher`. Response: `201 Created` with the device resource, including its role.

#### `GET /v1/devices`

Lists the authenticated identity's non-revoked devices. A `client` or `both` device of the owning identity MAY also call this: the query is scoped to the identity the device token already carries, so it reveals nothing that device could not infer from its terminal list, and it is what lets a paired client name the machine on which to ask for a terminal. `POST`, `GET /v1/devices/{device_id}` and `DELETE` remain identity-only. Pagination MUST use opaque cursors.

Each device resource additionally reports `publisher_connected` and `terminal_open_supported`, both booleans. The second is true only when the operator setting is enabled and the device's current connection asserted the capability of §4.6 condition 2.

#### `POST /v1/devices/{device_id}/terminals`

Asks the device's connected publisher to open a terminal. Requires `terminals:create` and the conditions of §4.6. `Idempotency-Key` is **required**, not merely supported: this operation causes a process to be created on another machine, and a retry MUST NOT create a second one. Two requests carrying the same key MUST resolve to one terminal even when they are concurrent.

Body fields, all optional: `label`, `cols`, `rows`. Unknown fields MUST be rejected. `cols` and `rows` are the initial geometry of a pseudo-terminal that does not yet exist, not a resize; the publisher remains the sole authority afterwards.

Response: `201 Created` with `Location: /v1/terminals/{terminal_id}` and the terminal resource of §5.3, plus `deduplicated`. `200 OK` with the same body when the publisher deduplicated the open. Errors: `feature_disabled`, `insufficient_scope`, `idempotency_key_required`, `invalid_request`, `validation_failed`, `not_found`, `publisher_unavailable`, `publisher_declined`, `limit_exceeded`, `rate_limited`, `idempotency_key_conflict`, `publisher_timeout`.

`publisher_timeout` is explicitly ambiguous: a terminal may still have been created. The caller MUST NOT resolve it by guessing; retrying under the same `Idempotency-Key` returns the real outcome, and refreshing the terminal list is always safe.

#### `GET /v1/devices/{device_id}`

Returns an owned device.

#### `DELETE /v1/devices/{device_id}`

Revokes an owned device. The operation is idempotent. Existing tokens and WebSockets for that device MUST be invalidated promptly and no later than 30 seconds after revocation.

### 5.3 Terminals

#### `GET /v1/terminals`

Lists terminals owned by the authenticated identity. Optional filters are `device_id`, `state`, and opaque pagination cursor.

#### `GET /v1/terminals/{terminal_id}`

Returns terminal metadata plus:

- `earliest_offset`: offset of the first byte still available for replay;
- `next_offset`: offset immediately after the last accepted byte; and
- `durable_offset`: offset immediately after the last byte committed to the database; and
- `retained_bytes`: `next_offset - earliest_offset`, always at most 1,500,000; and
- `accepts_input`: whether the publishing device opted in to receiving terminal input.

HTTP terminal *resources* are read-only: no request under `/v1/terminals` creates, mutates, or closes a terminal. Device publishers manage terminal lifecycle through their relay WebSocket so ordering between lifecycle and output events is explicit. `POST /v1/devices/{device_id}/terminals` (§5.2) does not create a terminal either: it asks that device's connected publisher to open one, and the terminal comes into existence only through the publisher's ordinary `terminal.open`. The ordering guarantee this paragraph exists to protect is therefore unchanged.

### 5.4 Operations

#### `GET /healthz`

Returns success if the process is alive. It MUST NOT depend on optional external services.

#### `GET /readyz`

Returns success only when the service can authenticate requests, read and write durable state, and accept relay traffic.

#### `GET /metrics`

MAY expose orchestration metrics. If exposed publicly, it MUST require operator authentication and MUST NOT contain terminal output, public keys, tokens, labels supplied by users, or other high-cardinality secrets.

### 5.5 Runtime settings

Runtime settings are operator-only resources. Operator authentication MUST be separate from identity and device authentication and MUST be suitable for deployment administration.

#### `GET /v1/admin/settings`

Returns the current settings revision and every defined setting with its name, type, effective value, default, allowed range or enum, description, sensitivity, and reload behavior. Secret values MUST be redacted; the response indicates only whether a secret or secret reference is configured.

#### `PATCH /v1/admin/settings`

Atomically updates one or more settings. The request MUST include the revision read by the operator. A stale revision returns `409 Conflict`; an invalid combination returns `422 Unprocessable Entity` without applying any part of the update.

Every successful change MUST be committed to the database in one transaction, assigned a new monotonically increasing revision, and recorded in an audit log with timestamp, operator principal, old-value hash, new-value hash, and outcome. Raw secret values MUST NOT be written to the audit log.

All healthy instances MUST observe a committed revision and apply it without a process restart within a database-configured propagation interval. Each request, connection, and output batch MUST use one immutable settings snapshot so that a concurrent update cannot produce internally inconsistent limits. Existing connections MAY keep connection-negotiated limits until reconnect only when the setting metadata explicitly declares that behavior; security revocations and reductions required to prevent resource exhaustion MUST apply to existing connections promptly.

## 6. WebSocket protocols

WebSocket clients MUST negotiate an explicit subprotocol:

- `terminal-relay.publisher.v1` — a device publishing terminals, output only;
- `terminal-relay.mirror.v1` — a client mirroring one terminal, output only;
- `terminal-relay.publisher.v2` — as v1, and additionally receives terminal input for its terminals; and
- `terminal-relay.mirror.v2` — as v1, and additionally may send terminal input.

A server MUST support all four. The version pair is independent: a v2 mirror may send
input only if the terminal's publisher is connected on v2, because a v1 publisher has
no frame in which to receive it. When a v2 mirror subscribes to a terminal whose
publisher cannot accept input, the server MUST report that in `subscribed` rather than
failing the subscription, so a client can attach read-only and say so in its UI.

A v1 peer MUST NOT observe any behaviour change from this document's version 2:
it never receives an input frame and never sees a version 2 control message.

Authentication MUST be completed during the HTTP upgrade using a bearer token, secure session cookie, or single-use WebSocket ticket. The server MUST reject missing, expired, path-mismatched, or insufficiently scoped credentials before upgrading.

Text frames contain UTF-8 JSON control messages. Binary frames contain terminal output. Unknown required message types or malformed frames cause an error message followed by close code `1002`. Application errors SHOULD use close codes in the private range `4000`–`4999` and be documented by the implementation.

### 6.1 Publisher connection

A device connects to:

```text
GET /v1/devices/{device_id}/relay
Sec-WebSocket-Protocol: terminal-relay.publisher.v1
```

After upgrade, the server sends `ready` with protocol limits and a `connection_id`.

The publisher may send these JSON control messages:

```json
{
  "type": "terminal.open",
  "request_id": "device-generated-id",
  "local_ref": "opaque-active-terminal-id",
  "label": "build shell",
  "cols": 120,
  "rows": 40,
  "term": "xterm-256color",
  "accepts_input": true
}
```

`accepts_input` defaults to false and is the publisher's opt-in to receiving terminal
input (§4.5). A publisher connected on subprotocol v1 MUST NOT set it, because it has
no frame in which input could be delivered; a server MUST reject such a request rather
than silently opening a terminal that can never be written to.

`in_reply_to`, when present, echoes the `request_id` of a `terminal.open_request` this open answers (§4.6). A version 1 publisher MUST NOT send it. A server MUST treat an `in_reply_to` that matches no pending request as absent — logging it and opening an ordinary publisher-initiated terminal — so that a publisher cannot attribute a terminal to a principal that never asked for one.

The server replies with `terminal.opened`, echoing `request_id` and returning `terminal_id`, `next_offset`, `durable_offset`, `accepts_input`, and whether the request was deduplicated.

A version 2 publisher MAY additionally send, at any time after `ready`:

```json
{ "type": "publisher.capabilities", "optional": true, "terminal_open_requests": true }
```

This asserts what the machine's owner currently allows, not what the build understands, which is why it is a message rather than a subprotocol. It is scoped to the connection: a reconnect starts from "not permitted" and MUST assert again, and a superseded connection MUST NOT be able to grant the capability to its replacement. A server that predates the message ignores it and simply never asks. A server MUST reject the assertion from a version 1 publisher with `validation_failed` while leaving the connection open.

The server asks a capable publisher to open a terminal with:

```json
{ "type": "terminal.open_request", "request_id": "opaque", "label": "build", "cols": 120, "rows": 40 }
```

It carries no command, argument vector, environment, working directory or `TERM`: the publishing machine alone decides what runs (§4.6). The publisher answers either with an ordinary `terminal.open` echoing `in_reply_to`, or with:

```json
{ "type": "terminal.open_declined", "in_reply_to": "opaque", "reason": "not_permitted", "detail": "optional, operator-facing" }
```

`reason` is one of `not_permitted`, `unsupported`, `busy`, `limit_reached`, or `internal_error`; a server MUST treat an unrecognised value as `internal_error`. `detail` is for the operator's log only and MUST NOT be forwarded to the requesting client.

```json
{
  "type": "terminal.resize",
  "terminal_id": "9ca8a5f0-1d27-4d77-af11-d40c420568d2",
  "cols": 160,
  "rows": 50
}
```

```json
{
  "type": "terminal.close",
  "terminal_id": "9ca8a5f0-1d27-4d77-af11-d40c420568d2",
  "reason": "process_exited"
}
```

Terminal output MUST be sent in binary frames with the following network-byte-order layout:

```text
byte 0       frame type: 0x01 (terminal output)
bytes 1-16   terminal UUID, 16 raw bytes
bytes 17-24  expected start offset, unsigned 64-bit integer
bytes 25..   opaque terminal output bytes
```

The output bytes are opaque: the server MUST NOT require UTF-8, parse ANSI escape sequences, normalize newlines, or otherwise transform them.

An output frame is accepted into memory only when its expected start offset equals the server's current `next_offset`. A mismatched frame MUST NOT be appended; the server returns `offset_mismatch` with the authoritative `next_offset` and `durable_offset`. This makes retries deterministic and prevents silent duplication after reconnects.

The server MAY immediately relay accepted in-memory bytes to mirror clients. It sends a cumulative `output.ack` only after a batch containing those bytes has committed to the database. The acknowledgement contains `terminal_id`, `durable_offset`, and the current in-memory `next_offset`; one acknowledgement MAY cover multiple input frames. The publisher MUST retain all bytes at or above the last acknowledged `durable_offset`. After reconnecting, it resumes at the `next_offset` returned by `terminal.opened`; if a server restart caused `next_offset` to fall back to `durable_offset`, this naturally retransmits the lost memory-only suffix. To limit memory use, the server MUST publish maximum frame size and maximum outstanding unacknowledged bytes in `ready`, apply backpressure, and close publishers that continue exceeding the negotiated limits.

Only one active publisher connection may control a given device at a time. A new authenticated connection SHOULD replace the older connection, which is closed with an application-specific `superseded` code.

### 6.2 Mirror connection

An identity client connects to:

```text
GET /v1/terminals/{terminal_id}/mirror
Sec-WebSocket-Protocol: terminal-relay.mirror.v1
```

The client then sends exactly one initial subscription message:

```json
{
  "type": "subscribe",
  "from_offset": 48120
}
```

`from_offset` is the offset of the next byte the client wants; all lower offsets have already been processed. It MAY be omitted to request the entire retained replay window. The server replies with:

```json
{
  "type": "subscribed",
  "terminal_id": "9ca8a5f0-1d27-4d77-af11-d40c420568d2",
  "requested_from_offset": 48120,
  "replay_start_offset": 48120,
  "next_offset": 51000,
  "durable_offset": 50600,
  "terminal_state": "open",
  "cols": 160,
  "rows": 50,
  "accepts_input": true,
  "input_available": true
}
```

`accepts_input` reports the publisher's declared opt-in. `input_available` reports
whether *this* subscription may send input right now: it is true only when every
condition in §4.5 currently holds, including a connected version 2 publisher. Both
fields are omitted for a version 1 subscriber. A client SHOULD present a read-only
state when `input_available` is false, and MUST NOT infer from `accepts_input` alone
that its keystrokes will be delivered.

If the requested offset is older than `earliest_offset`, the server MUST first send a `gap` control message containing `requested_from_offset` and `available_from_offset`, then replay from `earliest_offset`. If it is greater than `next_offset`, the subscription MUST fail with `offset_ahead` and include `next_offset` and `durable_offset`. This can occur after a server restart when a subscriber previously observed memory-resident bytes that the publisher has not yet replayed. There MUST be no gap or reordering between replay and live delivery.

Output is delivered in binary frames:

```text
byte 0       frame type: 0x01 (terminal output)
bytes 1-8    start offset, unsigned 64-bit integer
bytes 9..    opaque terminal output bytes
```

For a frame carrying `N` payload bytes at start offset `S`, the next expected offset is `S + N`. Zero-length output frames MUST NOT be sent. Bytes at offsets greater than or equal to `durable_offset` are live but not yet crash-durable. After each relevant batch commit, the server sends `{"type":"durable","durable_offset":51000}`. Clients that require durable processing SHOULD advance their persistent resume cursor only when this message raises `durable_offset`. Resize events are JSON `terminal.resize` messages. When a terminal closes, subscribers receive `terminal.closed` after all accepted output has been sent and committed; the WebSocket may then close normally.

The server MUST preserve byte ordering separately for each terminal. Slow subscribers MUST have a bounded outbound queue. When a subscriber exceeds that bound, the server MUST close it with a `slow_consumer` error; the client can reconnect using its last processed offset.

Both WebSocket protocols MUST use ping/pong or an equivalent heartbeat and close connections that remain unresponsive for a configurable interval.

### 6.3 Terminal input

Input travels from a version 2 mirror subscriber, through the relay, to the version 2
publisher connection that owns the terminal. The relay is a conduit: it MUST NOT
interpret, translate, normalize, echo, reorder, or persist input bytes.

A subscriber sends input in a binary frame:

```text
byte 0      frame type: 0x02 (terminal input)
bytes 1-8   client sequence, unsigned 64-bit integer
bytes 9..   opaque input bytes
```

The client sequence starts at 1 on each connection and increases by exactly one per
input frame. It exists so a client can learn precisely how much of its input was
accepted, which §7.3 of the client contract requires after an ambiguous disconnect. A
frame whose sequence is not the expected next value MUST be rejected with
`input_sequence_mismatch` carrying the expected value, and MUST NOT be delivered.
Zero-length input frames MUST NOT be sent and MUST be rejected.

The relay delivers accepted input to the publisher as:

```text
byte 0      frame type: 0x02 (terminal input)
bytes 1-16  terminal UUID, 16 raw bytes
bytes 17-24 relay input sequence, unsigned 64-bit integer
bytes 25..  opaque input bytes
```

The relay input sequence is per terminal, starts at 1 when the terminal is opened, and
increases by one per delivered frame regardless of which subscriber sent it. It lets a
publisher detect loss across a reconnect. It is not durable: it resets when a terminal
is reloaded from durable state, because unacknowledged input is never replayed.

The relay acknowledges input only after the frame has been handed to the publisher's
connection:

```json
{ "type": "input.ack", "accepted_through": 42, "relay_sequence": 913 }
```

`accepted_through` is cumulative over the subscriber's own client sequence. A client
MUST NOT treat unacknowledged input as delivered, and MUST NOT resend it on a new
connection: input is at-most-once, and a silent replay of a partially delivered
keystroke sequence is more dangerous than a lost one. A client that reconnects with
unacknowledged input SHOULD surface that to the user rather than guess.

Input MUST be refused, with an `error` message and no delivery, when:

| Condition | Code |
|---|---|
| The terminal did not opt in | `input_not_accepted` |
| The token lacks `terminals:input`, or the principal is not an authorized writer | `input_forbidden` |
| The operator disabled input | `input_disabled` |
| No version 2 publisher is connected for the device | `input_undeliverable` |
| The publisher's inbound queue is full | `input_backpressure` |
| The frame exceeds the negotiated maximum input frame size | `limit_exceeded` |
| The client sequence is not the expected next value | `input_sequence_mismatch` |

`input_undeliverable` and `input_backpressure` are transient: the subscription stays
open and the client may try again. The others are terminal for that subscription's
input authority and SHOULD be surfaced to the user.

Input MUST be rate limited per subscriber, by both frame rate and byte rate, using
database-backed settings. Exceeding the limit MUST fail the frame explicitly rather
than silently dropping it, because a dropped keystroke is invisible to the user.

A version 2 subscriber with input authority MAY also request a resize:

```json
{ "type": "terminal.resize_request", "cols": 100, "rows": 30 }
```

The relay forwards this to the publisher, which remains the sole authority over the
terminal's dimensions and reports the outcome with an ordinary `terminal.resize` seen
by every subscriber. A publisher MAY ignore the request. This keeps one owner of the
PTY size while still letting a phone that rotates ask for a size that fits its screen.
An operator MAY disable client-initiated resize independently of input.

## 7. Replay buffer and persistence

### 7.1 Exact capacity

For each terminal, the service MUST retain at most **1,500,000 bytes** of accepted terminal-output payload. This is decimal 1.5 MB, not 1.5 MiB. Protocol headers, database indexes, terminal metadata, and resize events do not count toward this payload limit, but implementations MUST keep their overhead bounded independently.

The buffer represents a contiguous suffix of the terminal's output stream:

```text
retained_bytes = next_offset - earliest_offset
0 <= retained_bytes <= 1,500,000
```

When appending would exceed the limit, the oldest bytes MUST be evicted from the in-memory replay buffer before or atomically with appending the new bytes. Eviction may split an originally received frame. If one output frame is larger than the limit, the service MUST accept it subject to the negotiated frame-size limit, advance `next_offset` by its full length, and retain only its last 1,500,000 bytes.

The replay capacity MUST be represented by the runtime setting `terminal.replay_capacity_bytes`, defaulting to 1,500,000. It MAY be tuned downward but MUST have a hard schema maximum of 1,500,000 so no database update can violate this specification.

### 7.2 Memory-first storage and database batching

For active terminals, the primary live and replay representation MUST be an in-memory bounded ring buffer. Accepting and relaying an ordinary output frame MUST NOT perform a synchronous database write. The hot path MUST NOT write one database row, transaction, or filesystem record per WebSocket frame.

Dirty output from multiple frames and, where supported by the database, multiple terminals MUST be coalesced into infrequent database transactions. A flush occurs when the first of these conditions is met:

- `persistence.flush_interval` elapses after the oldest dirty byte, default 5 seconds;
- total dirty output reaches `persistence.flush_bytes`, default 262,144 bytes;
- a terminal closes;
- graceful shutdown begins;
- memory pressure reaches the configured threshold; or
- an operator explicitly requests a flush.

Both periodic thresholds and the memory-pressure threshold are database-backed runtime settings. Implementations MUST enforce safe schema bounds; in particular, the flush interval MUST NOT be configured so high, and the dirty-byte limit MUST NOT be configured so large, that negotiated publisher retry windows or process memory bounds can be exceeded.

Each output batch transaction MUST persist a contiguous retained suffix, its `earliest_offset`, and its `durable_offset`. It SHOULD append each payload byte no more than once and SHOULD coalesce range deletion or compaction work instead of rewriting the entire 1,500,000-byte suffix on every flush. Database-specific write-ahead logging, fsync, checkpoint, and compaction policies MUST be selected to minimize physical disk writes while preserving the database's documented commit guarantee.

The in-memory `next_offset` advances as soon as bytes are accepted. `durable_offset` advances only after the transaction commits. The server MUST NOT send an `output.ack` beyond `durable_offset`. On commit failure, it MUST keep dirty bytes in memory, retry according to database-backed backoff settings, and apply publisher backpressure before dirty data could be lost through memory eviction. If persistence remains unavailable, readiness fails and publishers receive `storage_unavailable`; the server MUST never issue a false durable acknowledgement.

After a process or container restart, the service reconstructs buffers and offsets from the latest committed database state. Bytes relayed live after `durable_offset` may be absent until the publisher reconnects and retransmits them. Once an offset has been acknowledged as durable, offsets MUST NOT move backward, and acknowledged bytes that remain within the bounded retained suffix MUST survive restart.

Terminal input is never persisted. It is not terminal output, it does not enter the
replay buffer, it does not advance any offset, and it is not written to the database.
A subscriber that reconnects sees the remote terminal's echo of earlier input as
ordinary output, and nothing else. This keeps the durability model of §7 concerned
with exactly one stream, and keeps keystrokes — which include passwords — out of
durable storage entirely.

Security-critical mutations—including identity and device registration, device revocation, settings updates, signing-key state, and consumption of single-use credentials—MUST commit immediately when their API reports success. They MAY share a transaction with other pending security mutations, but MUST NOT wait for the terminal-output flush interval. This exception prevents the output hot path from forcing per-frame writes without weakening authentication or revocation correctness.

### 7.3 Retention lifecycle

Open terminals retain their bounded suffix. Closed terminals and their replay data MUST be retained for a configurable duration, defaulting to 24 hours after close, after which they MAY be deleted. Operators MUST be able to choose a shorter or longer duration and configure an overall storage quota.

When the overall quota is reached, the service SHOULD delete expired closed terminals first, then the oldest closed terminals. It MUST NOT silently reduce an open terminal below its configured per-terminal replay window merely to satisfy a global quota. If adequate storage cannot be maintained, readiness MUST fail and publishers MUST receive an explicit `storage_unavailable` error instead of false acknowledgements.

### 7.4 Atomicity

For a given terminal, appending bytes, advancing `next_offset`, and evicting old in-memory bytes MUST be atomic with respect to readers. Subscribers observe either the state before an append or the state after it, never partially updated offsets or a non-contiguous replay range. A database checkpoint MUST atomically publish its retained range and `durable_offset`; recovery MUST never combine payload from one checkpoint with offsets from another.

## 8. Container and operations requirements

The project MUST provide an OCI-compatible image and an example deployment configuration. The image SHOULD:

- run as a non-root user;
- use a minimal pinned base image;
- expose one configurable HTTP port, default `8080`;
- write logs to standard output/error;
- handle `SIGTERM`, stop accepting new connections, and drain active writes before the configured shutdown deadline;
- contain a container health check or document orchestration probes; and
- have a read-only root filesystem except for explicitly mounted temporary and data paths.

Durable state MUST live in a database backed by persistent storage. For an embedded database, its configurable data directory defaults to `/var/lib/terminal-relay` and MUST be a persistent volume. Deployments MUST NOT rely on the container's ephemeral writable layer for identities, devices, terminal metadata, committed offsets, challenges, revocations, settings, or replay checkpoints.

### 8.1 Database-backed runtime configuration

Every value that drives service behavior MUST have a typed setting stored in the database and MUST be runtime tunable through the operator settings API. This includes feature switches, defaults, timeouts, retention, replay capacity, persistence batching, limits, quotas, rate limits, retry/backoff policies, heartbeat behavior, graceful shutdown, logging levels, trusted-proxy behavior, public origin, token and challenge lifetimes, key-rotation policy, and listen/TLS behavior. Network and TLS changes MUST use a safe live rebind, connection drain, or credential-overlap sequence rather than requiring a process restart.

Settings MUST NOT be scattered as independently authoritative constants or environment variables. Code MAY contain initial defaults, protocol invariants, and hard safety/security bounds, but on first database initialization it MUST seed those defaults as database rows. Thereafter the database value is authoritative. A setting change MUST survive restart.

The settings schema MUST provide, for every behavior setting:

- stable dotted name and description;
- scalar or structured type;
- default and current value;
- validation constraints and cross-setting validation;
- whether it is secret or a reference to a secret;
- current revision and update timestamp; and
- reload behavior, including whether existing connections are renegotiated, drained, or immediately constrained.

Only values required to locate, decrypt, and authenticate to the settings database may be supplied as bootstrap environment variables, command-line arguments, or mounted secret files. For an embedded database this exception includes its path and encryption key; for an external database it includes its endpoint, database name, and credential or credential-file path. Instance identity and an emergency database-recovery mode MAY also be bootstrap values. Bootstrap values MUST NOT control normal relay, API, security-policy, retention, batching, or limit behavior.

The service MUST load and validate a complete settings snapshot before readiness succeeds. An invalid stored setting or an unsupported settings-schema version MUST fail readiness rather than silently falling back to a compiled or environment default. If a changed setting cannot be applied in place, the service MUST reject the update unless its declared reload behavior provides a safe automated drain/rebind sequence; merely documenting that an operator must restart the container does not satisfy runtime tunability.

Secret settings SHOULD store references to a secret provider rather than plaintext. If a secret must be stored in the database, it MUST be encrypted with bootstrap key material that is not stored in that database. Secret rotation MUST support an overlap period when required for active tokens or connections.

Single-instance deployment is sufficient for version 1. A multi-instance implementation MUST provide shared durable state, globally consistent per-terminal publishing/offset serialization, ticket consumption, revocation, and subscriber fan-out; sticky sessions alone are not sufficient correctness guarantees.

## 9. Observability and privacy

Every HTTP request and WebSocket connection MUST receive a correlation ID. Logs SHOULD be structured and include event type, correlation ID, authenticated principal ID, device or terminal ID when applicable, result, latency, and byte counts.

The service MUST NOT log:

- terminal-output payloads;
- terminal-input payloads, which routinely contain credentials as the user types them;
- access tokens, WebSocket tickets, challenges, or signatures;
- raw public keys; or
- request headers or bodies without redaction.

Metrics SHOULD include active publisher and mirror connections, terminals by state, accepted and delivered byte counts, replay bytes, dirty bytes, durable-offset lag, batch sizes, batch age, database transaction and physical-write counts where available, evictions, offset mismatches, slow-consumer disconnects, authentication failures, storage errors, active settings revision, settings propagation lag, and request latency. User-controlled labels and identity/device/terminal IDs MUST NOT be metric dimensions.

Terminal contents are sensitive data. Backups, persistent volumes, and transport endpoints MUST be protected accordingly. Encryption at rest SHOULD be supported, whether by the application or storage platform.

## 10. Limits and failure behavior

Implementations MUST define and enforce finite limits for identities per source during registration, devices per identity, active terminals per device, connections per principal, frame size, metadata field lengths, subscription queues, and total storage. Defaults SHOULD permit normal interactive terminals while resisting accidental or malicious resource exhaustion.

Failures MUST be explicit:

- Authentication or authorization failure: reject before WebSocket upgrade where possible.
- Invalid publisher offset: reject that frame without changing stored output.
- Durable-storage failure: do not acknowledge output.
- Subscriber falling behind in its outbound queue: disconnect it as a slow consumer.
- Undeliverable or unauthorized input: refuse that frame explicitly and leave the subscription open for transient causes. Never queue input for a disconnected publisher, and never drop it silently.
- Publisher disconnect: keep its terminals open during a configurable reconnect grace period; after the grace period, mark them closed with reason `publisher_disconnected`.
- Server shutdown: stop upgrades, finish committed writes, send a reconnecting/server-shutdown notice when possible, and close within the shutdown deadline.

Retries of mutating HTTP operations SHOULD support an `Idempotency-Key` header retained for at least 24 hours. Device terminal-open requests are deduplicated by the combination of device ID and active `local_ref`.

## 11. Acceptance criteria

An implementation conforms to this specification when automated tests demonstrate all of the following:

1. A new Ed25519 key can register only after signing a valid, unexpired, single-use challenge.
2. Re-registering the same key returns the same identity ID.
3. An identity can register and revoke multiple independently keyed devices.
4. A device cannot publish to another device's terminal, and one identity cannot discover or mirror another identity's resources.
5. A device can advertise zero terminals, one terminal, or multiple concurrently active terminals over its relay connection.
6. Arbitrary binary terminal output, including invalid UTF-8 and ANSI control bytes, is relayed without modification and in order.
7. A subscriber receives the retained replay followed by live bytes without duplication, loss, or reordering.
8. A reconnecting subscriber can resume from a processed offset, and receives an explicit gap notification if that offset has been evicted.
9. No terminal retains more than 1,500,000 output bytes; after more data arrives, the retained data is exactly the newest contiguous suffix.
10. Output is accepted into and served from a bounded in-memory ring without a database transaction per WebSocket frame; many frames are coalesced into substantially fewer database transactions under sustained load.
11. A cumulative output acknowledgement is sent only after its batch commits, and acknowledged retained output and monotonic offsets survive container restart with persistent database storage attached.
12. A crash before a batch commit rolls back only to `durable_offset`, after which the publisher can retransmit the memory-only suffix without duplication.
13. Duplicate publisher retries do not duplicate bytes, and offset mismatches do not mutate terminal state.
14. Slow consumers, oversized frames, excessive terminals, and rate-limit violations are bounded and fail explicitly.
15. Device revocation prevents new connections immediately and terminates existing access within 30 seconds.
16. Every behavior-driving value is represented by a typed database setting, valid updates apply without restart and survive restart, and invalid or conflicting updates are atomically rejected.
17. Concurrent requests use internally consistent settings snapshots, and all healthy instances converge on the committed settings revision within the configured propagation interval.
18. The supplied container runs as non-root, reports health/readiness correctly, stores durable state on a mounted volume, and shuts down gracefully after flushing dirty terminal output.
19. An authorized version 2 subscriber's input reaches the publishing device intact, in order, and exactly once per accepted frame, and the terminal's echo of it returns to every subscriber as ordinary output.
20. Input is refused, with a distinct code and no delivery, when the terminal did not opt in, the token lacks `terminals:input`, the caller does not own the terminal, an operator disabled input, or no version 2 publisher is connected.
21. Input never enters the replay buffer, durable storage, offsets, or logs, and a subscriber that reconnects replays only output.
22. Version 1 publishers and mirrors behave exactly as before alongside version 2 peers, and a version 1 publisher cannot open a terminal that claims to accept input.
23. A `client` device can mirror and write to its owner's terminals without ever holding the identity's root private key, and revoking it terminates that access.

## 12. Versioning

Breaking HTTP changes require a new base-path version. Breaking WebSocket changes require a new subprotocol name. New optional JSON fields and new ignorable control-message types may be added within version 1, but security-sensitive behavior must never depend on a peer silently ignoring a field or message it does not understand.

Terminal input was introduced under exactly that rule. It changes what a peer may send
and receive, so it required the new subprotocol names in §6 rather than an extension of
version 1. The HTTP surface stayed at `/v1` because its changes are additive — an
optional `role` on device registration, an `accepts_input` field on terminal
resources, and one further scope — and no existing response changed meaning. A
version 1 peer is unaffected in both cases.
