# Client integration

What a mirror client implements against this relay. `spec.md` is normative for the
relay and `../android/spec.md` for the Android client; this document is the contract
between them, and states the behaviour a client can rely on today.

The mirror is bidirectional: the phone sees the terminal, and what the phone types
appears on the remote terminal wherever it is running.

## 1. The model

An **identity** is a public key. Its ID is a deterministic fingerprint of that key —
there are no usernames and no passwords.

A **device** is a separately keyed principal owned by one identity, with a role of
`publisher`, `client`, or `both` (relay spec §3.2). A phone registers as a `client`:
its own key pair, its own revocable credential, no publishing authority and no
device-management authority. Revoking it ends its access within seconds and leaves the
workstation untouched. The identity's root private key never reaches the phone.

A **terminal** is one output stream from one publishing device. The relay carries
**raw ordered PTY bytes**: it performs no terminal emulation and has no notion of a
screen. All emulation is the client's.

Everything is addressed by a single monotonic **byte offset** per terminal. An offset
is a byte count, not a message counter: it advances by the payload length of each
frame.

The client's normalized events (client spec §7.2) map onto the wire as:

| Client concept | Relay reality |
|---|---|
| `Connected(session_id)` | `ready`, then `subscribed` after your `subscribe` |
| `Snapshot(revision, …)` | The replay frames following `subscribed`, starting at `replay_start_offset` |
| `Output(sequence, bytes)` | Binary frame: `0x01`, `u64` start offset, payload |
| `RemoteResize(cols, rows)` | `terminal.resize` |
| `Metadata(title, attrs)` | `label`, `cols`, `rows`, `term` in `subscribed`; no OSC title relay |
| `SessionClosed(reason)` | `terminal.closed` |
| `ProtocolError(...)` | `error`, then a close code |
| `Attach(last_revision, …)` | `subscribe` with `from_offset` |
| `Input(client_sequence, bytes)` | Binary frame: `0x02`, `u64` client sequence, payload |
| `Resize(cols, rows)` | `terminal.resize_request` — a request, not a command |
| `RequestSnapshot()` | Reconnect and `subscribe` without `from_offset` |
| `Detach()` | Close the WebSocket |

## 2. Authentication

Every authentication is a proof of possession of a private key, in two steps:

```
POST /v1/auth/challenges
  { "operation": "...", "key": { "algorithm": "ed25519", "public_key": "<base64url>" } }
  → 201 { challenge_id, challenge, signature_context, signing_input, expires_at }

sign the raw bytes of base64url-decode(signing_input) with the private key

POST /v1/identities   (register)   or   POST /v1/auth/tokens   (authenticate)
  { "challenge_id": "...", "signature": "<base64url>" }
```

`operation` is `register_identity`, `authenticate_identity`, `register_device` or
`authenticate_device`. Challenges are single-use, expire in ≤5 minutes, and are
consumed by the first verification attempt whether or not it succeeds — so never retry
a failed signature against the same challenge.

`signing_input` is a convenience: the exact bytes to sign. The server always recomputes
and verifies against its own derivation, so a client may either sign what it is given
or construct the length-prefixed encoding itself from relay spec §4.2.

Store the private key in the Android Keystore. Access tokens are bearer strings valid
for at most 15 minutes; keep them in memory only.

**There are no refresh tokens.** "Refresh" is re-running `authenticate_device` with the
stored key, which needs no user interaction. Do it before expiry, or on the first `401`.

**Logout** is discarding the token locally. To end access from the server side, the
owner revokes the device: `DELETE /v1/devices/{device_id}`. That takes effect
immediately for new connections and within 30 seconds for live ones.

### Pairing a phone

`POST /v1/devices` requires an identity token, which the phone does not have and should
never have. The flow is therefore:

1. The phone generates a key pair and displays its public key (a QR code is the obvious
   carrier).
2. On a machine that holds the identity key, the owner registers it: a `register_device`
   challenge for that public key bound to the owner's `identity_id`, signed by the
   *phone's* key, then `POST /v1/devices` with `"role": "client"` and an identity token.
   The phone signs the challenge; the owner authorizes the registration. Both parties
   must act, which is what makes the pairing meaningful.
3. The phone then authenticates on its own from then on.

`hypeterm-publish pair-code` packages step 2 as a short-lived code the owner reads off
one machine and types into the phone.

## 3. Finding and opening terminals

```
GET /v1/terminals?state=open            → paginated list, opaque cursors
GET /v1/terminals/{terminal_id}         → metadata, offsets, retained_bytes, accepts_input
```

A client device sees exactly the terminals its owning identity owns. Anything else
answers `404`, never `403`, so the response reveals nothing about existence.
Authorization to attach is the same check as visibility: if you can `GET` it, you can
mirror it.

A client may also ask one of its own machines to start a terminal:
`POST /v1/devices/{device_id}/terminals` (relay spec §5.2, §4.6). The relay forwards
the ask to that device's connected publisher and waits; the terminal comes into
existence through the publisher's ordinary `terminal.open`. No route under
`/v1/terminals` has a mutating method.

- The `terminals:create` scope is in no principal's default scopes. It is equal in
  gravity to `terminals:input` — a credential holding both is shell-equivalent on the
  publishing machine — so an operator grants it deliberately or not at all.
- `GET /v1/devices` is reachable by a `client` or `both` device, which is how the phone
  names the machine to ask. Every other method on that resource is identity-only.
- Each device resource reports `publisher_connected` and `terminal_open_supported`, so
  a client can grey out a machine rather than discover a 503 by trying.
- `Idempotency-Key` is **required**. A retry must not start a second shell, and
  `publisher_timeout` is deliberately ambiguous: re-ask with the same key.
- The request carries a label and a geometry and nothing else. No command, argv,
  environment, working directory or `TERM` — the publishing machine decides all of it,
  and a request carrying any of them is a `400` rather than a field that is ignored.

The gate is on the publishing machine (`hypeterm-publish remote-open`, off by default),
not on the phone: a setting an attacker holding the phone can flip is not a security
control.

## 4. Reaching the relay

The relay can deploy onto a tailnet (`just up tailscale`), which is the recommended
shape for a mobile client. The server is a node with a MagicDNS name and a real,
automatically renewed certificate:

```
wss://hypeterm-relay.example.ts.net/v1/terminals/{id}/mirror
```

For the client this means:

- **Certificate validation works normally.** No self-signed certificate, no trust-store
  exceptions, no pinning needed. Client spec §7.4's requirement that TLS validation is
  never bypassed in release builds is satisfiable as written.
- **No port forwarding, no public exposure.** The relay is reachable only from devices
  on the same tailnet.
- **The app must be a tailnet member.** Both ends join once — the server at deploy time,
  the app when the user signs it in. The embedded Tailscale library keeps that to a
  single in-app login rather than a separate install.
- **Discovery is a hostname.** The app lets the user enter, or be configured with, the
  MagicDNS name; there is nothing else to configure.

Tailscale terminates TLS and forwards raw TCP to the relay, so the WebSocket protocol is
unchanged end to end: no HTTP-level proxy sits in the path to rewrite headers, strip
query parameters, or mishandle the upgrade.

One consequence worth knowing: the relay cannot see per-client source addresses through
the forward, so per-source rate limits are shared. Per-principal limits, keyed by the
client's own token, are unaffected — another reason for the app to hold a `client`-role
device credential of its own rather than sharing one.

## 5. The mirror connection

```
GET wss://<origin>/v1/terminals/{terminal_id}/mirror
Sec-WebSocket-Protocol: terminal-relay.mirror.v2
Authorization: Bearer <access token>
```

Offer `terminal-relay.mirror.v2`; you may offer both versions, and the server selects
the highest it supports and echoes it in the handshake response. Read the selected value
rather than assuming.

Browsers cannot set `Authorization` on a WebSocket, and tokens must never appear in a
query string. Two alternatives exist: a `relay_session` cookie, or a single-use ticket

```
POST /v1/auth/websocket-tickets  { "path": "/v1/terminals/{id}/mirror" }
  → 201 { ticket, expires_at }        # ≤60 s, one path, consumed on first use
```

sent as the `x-relay-ticket` header or a `relay_ticket` cookie. Native Android clients
use `Authorization`.

Text frames are JSON control messages; binary frames are payload. Both directions are
fully specified in relay spec §6.

```
server → mirror (output)    byte 0 = 0x01, bytes 1-8 = u64 start offset, bytes 9..  = payload
mirror → server (input)     byte 0 = 0x02, bytes 1-8 = u64 client sequence, bytes 9.. = payload
```

**No compression is negotiated.** Do not enable `permessage-deflate`; there is nothing
to bound a decompression ratio against.

Sizes and heartbeat values are stated by the server in the `ready` message it sends
immediately after upgrade: `max_output_frame_bytes`, `max_input_frame_bytes`,
`max_control_message_bytes`, `heartbeat_interval_seconds`, `heartbeat_timeout_seconds`.
Read them rather than hard-coding. The server pings on the interval and closes a
connection silent for longer than the timeout (code 4009).

## 6. Output, offsets and replay

Output is addressed by byte offset. `subscribe` with `from_offset` = the offset of the
next byte you want; omit it for the whole retained window.

- Below the window: a `gap` control message precedes the replay, which starts at
  `available_from_offset`. Your screen state is stale; the correct response is to reset
  the emulator and render forward from there.
- Above `next_offset`: the subscription fails with `offset_ahead`. This is normal after
  a server restart, when you saw bytes that were never made durable. Resubscribe without
  `from_offset`.
- The replay-to-live boundary has no gap and no duplication, so there is nothing to
  deduplicate: apply every byte you receive, in order, exactly once.
- `durable` messages raise the crash-safe watermark. Advance a *persistent* resume
  cursor only on those; bytes above `durable_offset` are live but may not survive a
  relay crash.

The retained window is at most 1,500,000 bytes — decimal, not 1.5 MiB.

## 7. Input

Number your input frames from 1 on each connection, increasing by exactly one. The relay
replies

```json
{ "type": "input.ack", "accepted_through": 42, "relay_sequence": 913 }
```

only after the frame has been handed to the publisher's connection. `accepted_through`
is cumulative over *your* sequence.

**Input is at-most-once, and the relay never replays it.** After an ambiguous
disconnect, do not resend unacknowledged input: a partially delivered keystroke sequence
replayed into a live shell is worse than a lost one. Reject input generated while
disconnected, show the disconnected state, and extend the same treatment to input that
was sent but never acknowledged.

A refused frame does **not** consume its sequence number, so a frame rejected for a
transient reason (`input_undeliverable`, `input_backpressure`) may be retried with the
same sequence.

Whether a subscription may type is reported per subscription as `input_available`, and
is re-checked per frame against the four conditions of relay spec §4.5 — a client must
handle `input_available: false` by attaching read-only and saying so. The user-visible
states worth distinguishing are input refused (`input_not_accepted`, `input_forbidden`,
`input_disabled`) and input temporarily undeliverable (`input_undeliverable`,
`input_backpressure`).

## 8. Resize

The publishing device owns the PTY and therefore owns its dimensions. A client with
input authority sends `terminal.resize_request`, which the relay forwards; the publisher
applies it or ignores it, and the resulting `terminal.resize` reaches every subscriber.

Treat your requested size as a proposal: render at the size the server reports, and
debounce requests. An operator can disable client-initiated resize
(`features.client_resize_enabled`) independently of input.

## 9. Concurrent mirrors

The relay permits many simultaneous mirrors of one terminal — that is the point of a
mirror — and it does **not** arbitrate between multiple writers: frames are delivered
whole, in arrival order, never interleaved mid-frame.

A client that requires one attachment per session enforces that itself, using the
generation identifier client spec §11 mandates. A deployment needing a single writer
authorizes a single client device.

## 10. Encoding, `TERM` and locale

The relay guarantees only that bytes arrive unchanged and in order. `TERM` is declared by
the publishing device and reported in `subscribed`; for the supported profile it is
`xterm-256color`, and the remote environment must carry matching terminfo. Locale and
newline behaviour belong to the remote PTY, not the relay. Bracketed paste, mouse
reporting and focus reporting all work, because they are just bytes in each direction.

## 11. Terminal lifetime

A terminal stays open while its publisher is briefly disconnected
(`terminal.publisher_reconnect_grace_seconds`, default 60), so a flaky workstation
network does not destroy the session. After the grace period it closes with reason
`publisher_disconnected`.

When the shell exits, the publisher closes the terminal and the client receives
`terminal.closed` *after* every accepted byte has been delivered and committed. Closed
terminals and their replay data are retained for 24 hours by default and remain
readable, so a client can still show the final screen. A terminal ID is never reused.

## 12. Versioning

Minimum: any relay serving `/v1` and the `terminal-relay.mirror.v2` subprotocol.

- Breaking HTTP changes take a new base path; `/v1` is stable.
- Breaking WebSocket changes take a new subprotocol name; v1 and v2 are both served, and
  a version 1 peer observes no version 2 behaviour.
- New optional JSON fields and new *ignorable* control messages may appear within a
  version. **Ignore unknown fields.** For unknown control-message types, the rule is
  symmetric: treat a type you do not recognise as fatal unless it carries
  `"optional": true`.

Never make security-relevant behaviour depend on a peer silently ignoring something.

## 13. What the relay does not do

The relay is a byte conduit. It does not emulate terminals, allocate shells, execute
commands, transfer files, or decide which of several writers should win. Input is never
persisted, never enters the replay buffer, never advances an offset, and is never
logged — a password typed on the phone exists only in flight and in whatever the remote
terminal chooses to echo back.
