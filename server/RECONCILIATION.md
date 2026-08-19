# Client / server specification reconciliation

This resolves the conflicts between `spec.md` (the relay) and `../android/spec.md`
(the Terminal Mirror Android client), and answers the twelve open items the client
spec lists in its §18 as blockers for implementation freeze.

The decision that drives everything here: **the mirror is bidirectional**. The phone
sees the terminal, and what the phone types appears on the remote terminal wherever it
is running.

## 1. Conflicts and how each is resolved

### 1.1 Terminal input was out of scope — resolved by protocol version 2

The client spec requires input in §1, §3.5, §7.1, §9 and §17.3. The relay spec §2
excluded it, and permitted adding it later "only with an explicit authorization model
and protocol version change".

Both conditions are now met. Relay spec §4.5 defines the authorization model and §6.3
the wire protocol, carried on the new subprotocols `terminal-relay.publisher.v2` and
`terminal-relay.mirror.v2`. Version 1 is still served, and a version 1 peer observes
no change.

**Client action:** use `terminal-relay.mirror.v2`.

### 1.2 The client had no credential of its own — resolved by device roles

This was the more subtle blocker. Devices were publishers only, terminals are private
to the owning identity, and cross-identity sharing is out of scope. The phone could
therefore only reach the user's terminals by holding the identity's **root private
key** — exactly what relay spec §3.2 introduced device keys to avoid.

A device now has a role: `publisher`, `client`, or `both` (relay spec §3.2). The phone
registers as a `client`: its own key pair, its own revocable credential, no publishing
authority and no device-management authority. Revoking it ends its access within
seconds and leaves the workstation untouched.

**Client action:** generate a key pair in the Android Keystore and register it as a
`client` device. Never transport the identity key to the phone.

### 1.3 Two authoritative stream representations — resolved as PTY-stream mode

Client spec §8.3 requires choosing exactly one authoritative representation and
forbids mixing them. The relay only ever carries **raw ordered PTY bytes**; it does no
terminal emulation and has no notion of a screen.

**Client action:** adopt PTY-stream mode in §8.3 and delete the state-mirror option.
The Android client performs all terminal emulation.

### 1.4 Revisions and snapshots versus byte offsets

The client spec speaks of revisions, sequence numbers and snapshots. The relay uses a
single monotonic **byte offset** per terminal, and a "snapshot" is the retained replay
window rather than a parsed screen.

Mapping for the client's §7.2 normalized events:

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

**Client action:** rename `revision` to `offset` in the adapter. An offset is a byte
count, not a message counter: it advances by the payload length of each frame.

### 1.5 Resize ownership with several viewers

Client spec §10.3 publishes a resize whenever the rendered grid changes; §18.9 asks
who wins when several clients attach. The publishing device owns the PTY and therefore
owns its dimensions. A client with input authority sends `terminal.resize_request`,
which the relay forwards; the publisher applies it or ignores it, and the resulting
`terminal.resize` reaches every subscriber.

**Client action:** treat your requested size as a proposal. Render at the size the
server reports, and debounce requests as your §10.3 already requires. An operator can
disable client-initiated resize (`features.client_resize_enabled`) independently of
input.

### 1.6 Concurrent attachments and competing writers

Client spec §11 requires that reconnection not create concurrent attachments. The
relay permits many simultaneous mirrors of one terminal — that is the point of a
mirror — and it does **not** arbitrate between multiple writers: frames are delivered
whole, in arrival order, never interleaved mid-frame.

**Client action:** enforce one attachment per session yourself, using the generation
identifier §11 already mandates. If a deployment needs a single writer, authorize a
single client device.

### 1.7 "Register and authenticate users"

There are no usernames or passwords. An identity *is* a public key, and its ID is a
deterministic fingerprint of that key. See §2.2 below for the pairing flow.

**Client action:** replace "registration or sign-in" screens with key generation and
pairing. Client spec §12's warning about password contents becomes vacuous, though it
is harmless to keep.

### 1.8 The phone could see terminals but never start one — resolved by asking the machine

Terminals were publisher-initiated only, so a phone could mirror what was already
running and nothing else. Opening one is now `POST /v1/devices/{device_id}/terminals`
(server §5.2, §4.6): the relay forwards the ask to that device's connected publisher
and waits, and the terminal still comes into existence only through the publisher's
ordinary `terminal.open`. Nothing about the lifecycle-versus-output ordering changed,
and no route under `/v1/terminals` gained a mutating method.

The client-visible consequences:

- A new scope, `terminals:create`, in no principal's default scopes. It is equal in
  gravity to `terminals:input` — a credential holding both is shell-equivalent on the
  publishing machine — so an operator grants it deliberately or not at all.
- `GET /v1/devices` is now reachable by a `client` or `both` device, which is how the
  phone names the machine to ask. Every other method on that resource stays
  identity-only.
- Each device resource reports `publisher_connected` and `terminal_open_supported`, so
  a client can grey out a machine rather than discover a 503 by trying.
- `Idempotency-Key` is *required*, not merely supported. A retry must not start a second
  shell, and `publisher_timeout` is deliberately ambiguous: re-ask with the same key.
- The request carries a label and a geometry and nothing else. No command, argv,
  environment, working directory or `TERM` — the publishing machine decides all of it,
  and a request carrying any of them is a 400 rather than a field that is ignored.

The real gate is on the publishing machine (`hypeterm-publish remote-open`, off by
default), not on the phone: a setting an attacker holding the phone can flip is not a
security control.

## 2. Answers to the client spec's §18 open items

### 2.1 Registration and authentication endpoints, challenge flows, token storage

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

### 2.2 Token refresh, revocation, logout, device registration

**There are no refresh tokens.** "Refresh" is simply re-running
`authenticate_device` with the stored key, which needs no user interaction. Do it
before expiry, or on the first `401`.

**Logout** is discarding the token locally. To end access from the server side, the
owner revokes the device: `DELETE /v1/devices/{device_id}`. That takes effect
immediately for new connections and within 30 seconds for live ones.

**Pairing a phone.** `POST /v1/devices` requires an identity token, which the phone
does not have and should never have. The flow is therefore:

1. The phone generates a key pair and displays its public key (a QR code is the
   obvious carrier).
2. On a machine that holds the identity key, the owner registers it:
   a `register_device` challenge for that public key bound to the owner's
   `identity_id`, signed by the *phone's* key, then `POST /v1/devices` with
   `"role": "client"` and an identity token.
   The phone signs the challenge; the owner authorizes the registration. Both parties
   must act, which is what makes the pairing meaningful.
3. The phone then authenticates on its own from then on.

This needs no protocol addition. A server-issued short-lived pairing code would be a
usability improvement and can be added later without breaking anything.

### 2.3 Session discovery and attach authorization

```
GET /v1/terminals?state=open            → paginated list, opaque cursors
GET /v1/terminals/{terminal_id}         → metadata, offsets, retained_bytes, accepts_input
```

A client device sees exactly the terminals its owning identity owns. Anything else
answers `404`, never `403`, so the response reveals nothing about existence.
Authorization to attach is the same check as visibility: if you can `GET` it, you can
mirror it.

### 2.3a Reaching the server: Tailscale

The relay can deploy onto a tailnet (`just up tailscale`), which is the recommended
shape for a mobile client. The server becomes a node with a MagicDNS name and a real,
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
- **The app must be a tailnet member.** Both ends join once — the server at deploy
  time, the app when the user signs it in. Embedding the Tailscale library in the app
  keeps that to a single in-app login rather than a separate install.
- **Discovery is a hostname.** The app should let the user enter, or be configured
  with, the MagicDNS name; there is nothing else to configure.

Tailscale terminates TLS and forwards raw TCP to the relay, so the WebSocket protocol
is unchanged end to end: no HTTP-level proxy sits in the path to rewrite headers,
strip query parameters, or mishandle the upgrade.

One consequence worth knowing: the relay cannot see per-client source addresses
through the forward, so per-source rate limits are shared. Per-principal limits, keyed
by the client's own token, are unaffected — another reason for the app to hold a
`client`-role device credential of its own rather than sharing one.

### 2.4 WebSocket URL, headers, subprotocol, version negotiation

```
GET wss://<origin>/v1/terminals/{terminal_id}/mirror
Sec-WebSocket-Protocol: terminal-relay.mirror.v2
Authorization: Bearer <access token>
```

Offer `terminal-relay.mirror.v2`; you may offer both versions, and the server selects
the highest it supports and echoes it in the handshake response. Read the selected
value rather than assuming.

Browsers cannot set `Authorization` on a WebSocket, and tokens must never appear in a
query string. Two alternatives exist: a `relay_session` cookie, or a single-use ticket

```
POST /v1/auth/websocket-tickets  { "path": "/v1/terminals/{id}/mirror" }
  → 201 { ticket, expires_at }        # ≤60 s, one path, consumed on first use
```

sent as the `x-relay-ticket` header or a `relay_ticket` cookie. Native Android clients
should just use `Authorization`.

### 2.5 Authoritative payload: raw PTY bytes

Raw PTY bytes. Always. The relay never parses, transforms, normalizes newlines,
validates UTF-8, or interprets escape sequences in either direction.

### 2.6 Message schemas, framing, compression, sizes, heartbeat

Text frames are JSON control messages; binary frames are payload. Both directions are
fully specified in relay spec §6.

```
server → mirror (output)    byte 0 = 0x01, bytes 1-8 = u64 start offset, bytes 9..  = payload
mirror → server (input)     byte 0 = 0x02, bytes 1-8 = u64 client sequence, bytes 9.. = payload
```

**No compression is negotiated.** Do not enable `permessage-deflate`; there is nothing
to bound a decompression ratio against.

Sizes and heartbeat values are not guesswork — the server states them in the `ready`
message it sends immediately after upgrade: `max_output_frame_bytes`,
`max_input_frame_bytes`, `max_control_message_bytes`, `heartbeat_interval_seconds`,
`heartbeat_timeout_seconds`. Read them rather than hard-coding. The server pings on the
interval and closes a connection silent for longer than the timeout (code 4009).

### 2.7 Sequence, acknowledgement, replay, deduplication, snapshots

Output is addressed by byte offset. `subscribe` with `from_offset` = the offset of the
next byte you want; omit it for the whole retained window.

- Below the window: a `gap` control message precedes the replay, which starts at
  `available_from_offset`. Your screen state is stale; the correct response is to
  reset the emulator and render forward from there.
- Above `next_offset`: the subscription fails with `offset_ahead`. This is normal after
  a server restart, when you saw bytes that were never made durable. Resubscribe
  without `from_offset`.
- The replay-to-live boundary has no gap and no duplication, so there is nothing to
  deduplicate: apply every byte you receive, in order, exactly once.
- `durable` messages raise the crash-safe watermark. Advance a *persistent* resume
  cursor only on those; bytes above `durable_offset` are live but may not survive a
  relay crash.

The retained window is at most 1,500,000 bytes — decimal, not 1.5 MiB.

### 2.8 Input acknowledgement, especially after an ambiguous disconnect

Number your input frames from 1 on each connection, increasing by exactly one. The
relay replies

```json
{ "type": "input.ack", "accepted_through": 42, "relay_sequence": 913 }
```

only after the frame has been handed to the publisher's connection. `accepted_through`
is cumulative over *your* sequence.

**Input is at-most-once, and the relay never replays it.** After an ambiguous
disconnect, do not resend unacknowledged input: a partially delivered keystroke
sequence replayed into a live shell is worse than a lost one. Your §9.3 policy —
reject input generated while disconnected and show the disconnected state — is exactly
right; extend it to input that was sent but never acknowledged.

A refused frame does **not** consume its sequence number, so a frame rejected for a
transient reason (`input_undeliverable`, `input_backpressure`) may be retried with the
same sequence.

### 2.9 Resize ownership when multiple clients attach

See §1.5. The publisher owns the size; you send `terminal.resize_request`.

### 2.10 Encoding guarantees, `TERM`, locale, newlines, extensions

The relay guarantees only that bytes arrive unchanged and in order. `TERM` is declared
by the publishing device and reported to you in `subscribed`; for the supported profile
it is `xterm-256color`, and the remote environment must carry matching terminfo. Locale
and newline behaviour belong to the remote PTY, not the relay. Bracketed paste, mouse
reporting and focus reporting all work, because they are just bytes in each direction.

### 2.11 Session persistence and remote shell exit

A terminal stays open while its publisher is briefly disconnected
(`terminal.publisher_reconnect_grace_seconds`, default 60), so a flaky workstation
network does not destroy the session. After the grace period it closes with reason
`publisher_disconnected`.

When the shell exits, the publisher closes the terminal and you receive
`terminal.closed` *after* every accepted byte has been delivered and committed. Closed
terminals and their replay data are retained for 24 hours by default and remain
readable, so a client can still show the final screen. A terminal ID is never reused.

### 2.12 Minimum server version and compatibility policy

Minimum: any relay serving `/v1` and the `terminal-relay.mirror.v2` subprotocol.

- Breaking HTTP changes take a new base path; `/v1` is stable.
- Breaking WebSocket changes take a new subprotocol name; v1 and v2 are both served.
- New optional JSON fields and new *ignorable* control messages may appear within a
  version. **Ignore unknown fields.** For unknown control-message types, the rule is
  symmetric: treat a type you do not recognise as fatal unless it carries
  `"optional": true`.

Never make security-relevant behaviour depend on a peer silently ignoring something.

## 3. Changes the client specification needs

1. **§8.3** — select PTY-stream mode; delete state-mirror mode.
2. **§7.2** — rename `revision`/`sequence` to `offset`; map events per §1.4 above.
3. **§5.1, §3.1** — replace registration/sign-in with key generation plus the pairing
   flow in §2.2; there are no passwords.
4. **§7.1** — "Keyboard input publication" is satisfied, on `mirror.v2`, subject to the
   four conditions of relay spec §4.5. Add handling for `input_available: false`, when
   the client must attach read-only and say so.
5. **§10.3** — resize is a request the publisher may decline, not a publication.
6. **§7.3** — add: never resend unacknowledged input after a reconnect.
7. **§7.4** — state that no compression is negotiated.
8. **§11** — note that the relay permits multiple concurrent mirrors and does not
   arbitrate between writers; single-attachment discipline is the client's job.
9. **§12** — add: the client credential is a `client`-role device key held in the
   Keystore; the identity key never reaches the device.
10. **§15** — add two user-visible states: input refused (`input_not_accepted`,
    `input_forbidden`, `input_disabled`) and input temporarily undeliverable
    (`input_undeliverable`, `input_backpressure`).

## 4. What did not change

The relay remains a byte conduit. It does not emulate terminals, allocate shells,
execute commands, transfer files, or decide which of several writers should win. Input
is never persisted, never enters the replay buffer, never advances an offset, and is
never logged — a password typed on the phone exists only in flight and in whatever the
remote terminal chooses to echo back.
