# Protocol integration

How the client speaks to the Terminal Mirror Relay. The relay's specification is
`../server/spec.md`; `../server/INTEGRATION.md` is the contract between the two. Everything here lives in `core/src/api/`, and nothing outside
that directory depends on it (spec §7.1).

## Identity, pairing and tokens

There are no usernames or passwords. An identity *is* an Ed25519 public key, and its ID
is a deterministic fingerprint of that key:

```
identity_id = base64url(SHA-256(lp("terminal-relay-identity-v1") || lp(algorithm) || lp(key)))
```

The phone holds a **`client`-role device key**, never the identity key
(integration §2). Pairing therefore takes two parties:

1. The phone generates a key pair (`Controller::BeginPairing`) and shows the public key.
2. On a machine holding the identity key, the owner requests a `register_device`
   challenge bound to that identity, has the *phone's* key sign it, and posts
   `/v1/devices` with `"role": "client"`.
3. The phone records the resulting identity and device IDs
   (`Controller::CompletePairing`) and authenticates on its own from then on.

Authentication is a proof of possession, twice per token: request a challenge, sign the
returned `signing_input`, exchange it for a bearer token. There are no refresh tokens —
re-running `authenticate_device` with the stored key *is* the refresh, and it needs no
user interaction. Tokens last at most fifteen minutes and never leave memory.

### The client verifies what it signs

`crypto::VerifySigningInput` decodes the server-supplied `signing_input` and checks its
context, challenge ID, challenge bytes, operation and key fingerprint against what the
client asked for, *before* the key is used. A relay that returns a challenge binding a
different operation gets no signature. The origin field is checked when it matches the
configured base URL and tolerated when it does not, because a deployment behind a proxy
legitimately advertises a different public origin.

## Attaching

```
GET wss://<origin>/v1/terminals/{terminal_id}/mirror
Sec-WebSocket-Protocol: terminal-relay.mirror.v2, terminal-relay.mirror.v1
Authorization: Bearer <token>
```

Both versions are offered and the selected one is read from the handshake response. A
version 1 deployment has no frame in which input could travel, so the client attaches
read-only and says so rather than failing (`MirrorSession::protocol_v2()`).

No extensions are negotiated. `permessage-deflate` is deliberately not offered: without
a negotiated compressor there is no decompression ratio to bound (spec §7.4), and the
relay states that it compresses nothing.

Token material never appears in a URL. A single-use `x-relay-ticket` is supported for
deployments whose proxy strips `Authorization`, but native clients use the header.

## Offsets, not revisions

An offset is a **byte count**, not a message counter. The client tracks three:

| Value | Meaning | Where it lives |
| --- | --- | --- |
| `next_expected_offset` | The next byte the client wants | `MirrorSession`, in memory |
| `durable_offset` | Everything below this survived a relay crash | `MirrorSession` + `Preferences` |
| `replay_start_offset` | Where the current subscription's replay began | reported in `subscribed` |

`subscribe` is sent exactly once per connection:

- **Cold attach** (new session, or after a reset): no `from_offset`, so the whole
  retained window replays and the screen is rebuilt from authoritative bytes. This is
  what spec §7.3 means by receiving a snapshot on initial connection — the relay's
  "snapshot" is its replay window.
- **Warm reconnect** (same process, emulator state intact): `from_offset` = the last
  processed offset, so nothing is replayed twice.

Only a `durable` message advances the *persistent* cursor. Bytes above `durable_offset`
are live but not yet crash-durable, so persisting them would make the client resume
from bytes the relay may no longer have.

### Frame handling

```
server → mirror (output)   byte 0 = 0x01, bytes 1-8 = u64 start offset, bytes 9..  = payload
mirror → server (input)    byte 0 = 0x02, bytes 1-8 = u64 client sequence, bytes 9.. = payload
```

Note this differs from the publisher layout, which inserts a 16-byte terminal UUID.
Crossing the two would silently misparse every frame.

`MirrorSession::HandleBinaryFrame` applies three rules, in order:

1. A frame starting **after** `next_expected_offset` is a gap the relay promised could
   not happen: it is reported as a sync failure rather than rendered (spec §7.3).
2. A frame lying wholly **below** it is a duplicate and applies nothing.
3. A frame **overlapping** it applies only the new suffix.

## Failure paths

| Relay behaviour | Client response |
| --- | --- |
| `gap` | Reset the emulator, resume from `available_from_offset`, tell the user the screen was rebuilt |
| `offset_ahead` | Drop the resume offset, reset, resubscribe for the whole window |
| `slow_consumer` | Treat as a sync failure and reconnect from the last processed offset |
| `terminal.closed` | Surface it, stop reconnecting, leave the final screen readable |
| Close code 4001/4006 | Authentication failure; clear the token and re-authenticate |
| Silence past `heartbeat_timeout_seconds` | Treat the connection as dead and reconnect |

Reconnect uses exponential backoff with jitter that only ever *reduces* the delay, and
the sequence resets only after a connection has survived the stability threshold — a
server that accepts and instantly drops connections must not be hammered at the base
interval.

## Input

Input frames are numbered from 1 on each connection and increase by exactly one per
**accepted** frame. The relay's expected sequence is therefore always
`accepted_through + 1`, which is how `MirrorSession::ResetInputSequencing` recovers
after a refusal without parsing the human-readable error message.

Four independent conditions must hold before the relay delivers a byte (relay §4.5), so
`input_available` in `subscribed` — not `accepts_input` — decides whether this client
may type. When it is false the client refuses locally and says why, rather than sending
frames the relay would reject.

**Unacknowledged input is never resent.** After an ambiguous disconnect the client
tells the user that some input may not have arrived and moves on; replaying a partially
delivered keystroke sequence into a live shell is worse than losing it.

Terminal replies the emulator generates (DA, DSR, CPR) travel as input too, and are
dropped silently on a read-only attachment: they answer the remote's own query, so
there is nothing to report to the user.

## Resize

The publishing device owns the PTY dimensions. The client renders at the size reported
in `subscribed` and `terminal.resize`, and its own layout produces a
`terminal.resize_request` — a proposal the publisher may decline. Requests are debounced
so a rotation does not produce a storm, and the final size is always sent.

## Version compatibility

Unknown JSON *fields* are ignored. An unknown control-message *type* is fatal unless it
carries `"optional": true` — the same rule the relay applies in the other direction, so
security-relevant behaviour never depends on a silent skip.
