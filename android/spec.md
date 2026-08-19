# Terminal Mirror for Android — Specification

Status: Draft 0.2 — server integration resolved  
Target platform: Android 10 (API 29) or later  
Primary implementation language: C++17 or later  
Rendering API: OpenGL ES 3.0 or later

The supplied server API is the **Terminal Mirror Relay**, specified in
`../server/spec.md`. Where this document previously left integration open, it now
states the resolved behaviour; `../server/RECONCILIATION.md` records how each conflict
between the two documents was settled and is the reference for anything protocol-shaped
that this document summarises.

## 1. Purpose

Terminal Mirror is an Android client that displays and controls a terminal session running on a remote system. The application connects to a supplied server API, receives the remote terminal's output or synchronized terminal state over a WebSocket, renders it as an interactive terminal, and sends user keyboard input back to the remote terminal.

The app is a terminal mirror, not a local shell. It does not execute `/bin/bash` or other remote commands on the Android device.

## 2. Terminology

- **Remote terminal**: The shell or terminal process hosted outside the Android device.
- **Session**: One remotely hosted terminal instance that the user may attach to.
- **Terminal byte stream**: UTF-8 text mixed with ANSI/ECMA-48 control sequences, as normally emitted through a Unix pseudo-terminal (PTY).
- **Terminal state**: The parsed screen grid, cursor, modes, attributes, scrollback, and related metadata at a particular revision.
- **Server API**: The externally supplied registration, authentication, session discovery, and WebSocket protocol.

The phrase “`/bin/bash` type encodings” is interpreted in this specification as the byte stream commonly produced by Bash through a Unix PTY: UTF-8 text plus ANSI/VT-compatible escape and control sequences. Bash itself does not define a separate text encoding.

## 3. Goals

The first release shall:

1. Register and authenticate users through the supplied server API.
2. List or select terminal sessions as supported by that API.
3. Attach to one remote terminal through a secure WebSocket.
4. Display the terminal accurately and with low perceived latency.
5. Send soft-keyboard, hardware-keyboard, and terminal control-key input to the remote session.
6. Survive transient network loss and restore an authoritative terminal state after reconnecting.
7. Support typical interactive Bash programs, including full-screen terminal applications.

## 4. Non-goals for the first release

- Running a local shell or shipping `/bin/bash` on Android.
- File transfer, port forwarding, or general SSH functionality.
- Multiple visible terminal panes.
- User-provided terminal plug-ins or scripts.
- Perfect emulation of every historical terminal type.
- Server-side protocol, authentication, or session-host implementation.

## 5. Product behavior

### 5.1 Application flow

1. On first launch, the app presents **pairing**. There are no usernames or passwords: an identity is a public key. The app generates its own Ed25519 key pair, displays the public half, and the owner registers it as a `client`-role device from a machine that already holds the identity key. The app then records the returned identity and device IDs. The identity's private key shall never reach the device.
2. After pairing, it authenticates with its own device key and presents the remote sessions available to the user. If the API exposes only one session, the app may attach to it directly.
3. Selecting a session opens a terminal screen and establishes the WebSocket connection.
4. The terminal requests keyboard focus. Tapping it opens the Android input method editor (IME).
5. Incoming remote data is parsed and rendered as soon as practical.
6. User input is encoded and sent in order to the attached remote session.
7. If connectivity is interrupted, the visible terminal remains readable, input is temporarily disabled or clearly marked as pending, and the app attempts to reconnect.
8. When reconnection succeeds, the client obtains an authoritative snapshot or resumes from a confirmed revision before accepting further input.

### 5.2 Terminal screen

The terminal screen shall include:

- A monospaced terminal viewport.
- A cursor whose appearance reflects the active terminal mode when supplied.
- Vertical scrollback navigation by touch.
- An action that asks one of the user's own machines to open a new terminal, when the
  relay and that machine both allow it (relay spec §4.6). The request SHALL carry no
  command, arguments, environment or working directory: what runs is the far machine's
  decision. The client SHALL send an idempotency key and reuse it when retrying, because
  the operation starts a process and an ambiguous timeout MUST NOT be resolved by asking
  again with a fresh key.
- A control that returns to the newest output and follows it as it arrives, shown only while the view is not already following. Any gesture that moves the view away — a zoom, a pan, a scroll into the history, a selection — SHALL end following, because each of them is a statement that the user is reading something other than the live bottom. Following SHALL move the view only, never the zoom, and never the remote terminal.
- A compact extra-key row for keys difficult to enter with a mobile IME, at minimum: `Esc`, `Tab`, `Ctrl`, `Alt`, arrow keys, and `Enter`.
- A connection-state indicator that does not unnecessarily obscure terminal contents.
- An explicit action to reconnect or leave the session.

Selection and clipboard copy should be included in the first release. Paste shall require an intentional user action and shall use bracketed paste mode when the remote terminal enables it.

## 6. Technical architecture

The application shall use a small Android platform layer and a predominantly native C++ core.

### 6.1 Components

| Component | Responsibility |
|---|---|
| Android host layer | Activity lifecycle, window/surface ownership, IME integration, clipboard, accessibility bridge, secure credential storage, connectivity notifications |
| Native application controller | Screen state, session lifecycle, event routing, reconnect policy, coordination of all native modules |
| API adapter | Registration, authentication, token refresh, session discovery, and translation of the supplied server protocol into internal events |
| WebSocket transport | TLS connection, framing, ping/pong, backpressure, ordered delivery, reconnection, protocol errors |
| Terminal emulator | Incremental parsing of PTY bytes, terminal state machine, screen buffers, modes, scrollback, cursor, and resize behavior |
| Input encoder | Translation of Android text/key events into UTF-8 bytes and VT-compatible key sequences |
| OpenGL renderer | Glyph atlas, grid composition, colors, cursor, selection, clipping, scrolling, and frame presentation |
| Persistence layer | Non-secret preferences and bounded cached metadata; credentials are delegated to Android secure storage |

A minimal Java or Kotlin bridge is acceptable where Android does not expose required platform features conveniently through native APIs, especially IME composition, accessibility, clipboard, and Keystore access. Terminal logic, networking orchestration, protocol translation, and rendering shall remain in C++.

### 6.2 Threading model

- The Android UI thread owns platform callbacks and IME interaction.
- A render thread owns the EGL context and all OpenGL calls.
- A network/event thread owns the WebSocket and API I/O.
- Terminal parsing may run on the network/event thread or a dedicated parser thread, but it shall never mutate state currently being rendered.
- The renderer consumes immutable snapshots or revisioned change sets passed through bounded queues.
- User input is placed on an ordered outbound queue and is never sent directly from a UI callback.

Queues shall be bounded. Terminal output may be coalesced between frames, but input, resize, snapshot, and protocol-control messages shall not be silently discarded.

## 7. Server integration contract

The supplied API is the Terminal Mirror Relay (`../server/spec.md`), and it is authoritative. This section states how the client integrates with it. All knowledge of its wire format shall be confined to the API adapter.

### 7.1 Required API capabilities

The relay provides, and the client uses:

- **Identity registration** by proof of possession of a public key. There is no user registration in the conventional sense and no password.
- **Device registration and revocation**, so the client holds a `client`-role device key of its own rather than the identity's key.
- **Authentication** by signing a short-lived, single-use challenge. There are no refresh tokens: re-authenticating with the stored device key *is* the refresh, and it requires no user interaction. Sign-out is discarding the local token; ending access from the server side means revoking the device.
- **Session discovery**: `GET /v1/terminals` and `GET /v1/terminals/{id}`. Visibility and attach authorization are the same check, and a non-owner receives `404`, never `403`.
- **A `wss://` mirror endpoint** authenticated by a bearer token in a header, or by a single-use path-bound ticket. Token material shall never appear in a URL.
- **Terminal output delivery** as raw ordered PTY bytes addressed by monotonic 64-bit byte offsets.
- **Keyboard input publication** on subprotocol version 2, subject to the four independent conditions in relay spec §4.5. The client shall treat `input_available` in the subscription reply — not `accepts_input` — as the decision on whether it may type, and shall present a read-only state when it is false.
- **Terminal resize** as a *request* the publishing device may decline (§10.3).
- **Initial synchronization** as the relay's bounded replay window, with an explicit `gap` message when the requested offset has been evicted and an `offset_ahead` failure when it is ahead of the stream.
- **Session closure and error semantics**, including the input refusal codes in relay spec §6.3.
- **Protocol limits** — maximum frame sizes, control-message size and heartbeat intervals — which the relay states in its `ready` message. The client shall read them rather than hard-code them.

The API adapter shall expose a stable internal C++ interface so changes to the supplied API do not affect terminal parsing or rendering.

### 7.2 Internal normalized events

The adapter normalizes incoming messages into events equivalent to:

```text
Ready(limits)
Subscribed(terminal_id, replay_start_offset, next_offset, durable_offset,
           terminal_state, columns, rows, accepts_input, input_available)
Output(terminal_id, start_offset, bytes)
Gap(terminal_id, requested_from_offset, available_from_offset)
Durable(terminal_id, durable_offset)
RemoteResize(terminal_id, columns, rows)
TerminalClosed(terminal_id, reason)
InputAck(accepted_through, relay_sequence)
ProtocolError(code, recoverable, message)
```

Outbound operations are equivalent to:

```text
Subscribe(terminal_id, from_offset?)
Input(client_sequence, bytes)
RequestResize(columns, rows)
Detach()
```

An **offset is a byte count**, not a message counter: it advances by the payload length of each frame. There is no revision counter and no parsed-state snapshot; the authoritative state after a gap is reconstructed by replaying bytes.

### 7.3 Ordering and synchronization

- Terminal output shall be applied exactly in server-defined offset order.
- The client shall detect an offset discontinuity rather than render potentially corrupt state indefinitely. The relay guarantees contiguity between replay and live delivery, so a forward jump is a protocol failure and shall be surfaced.
- Bytes below the next expected offset are duplicates and shall not be applied twice; a partially overlapping frame contributes only its new suffix.
- On initial connection, or after an unrecoverable gap, the client shall subscribe without a starting offset so the whole retained window replays and the screen is rebuilt from authoritative bytes.
- After reconnecting within a session whose terminal state is intact, the client shall resume from its last processed offset.
- The client shall advance a **persistent** resume cursor only when a `durable` message raises `durable_offset`. Bytes above it are live but not yet crash-durable.
- The client shall not guess whether input was accepted after a connection failure. Input is at-most-once and the relay never replays it: **unacknowledged input shall never be resent on a new connection**, and the client shall surface that some input may not have been delivered.

### 7.4 Transport

- Production connections shall use HTTPS and `wss://` with normal certificate and hostname validation. Cleartext is permitted only to a loopback host, for development.
- TLS validation shall not be bypassed in release builds. Supplying additional trust anchors — which Android requires, because its trust store is not readable by the TLS library — is not a bypass; disabling verification is, and no such option shall exist.
- WebSocket binary frames carry PTY byte data in both directions; JSON text frames carry control messages.
- **No compression is negotiated.** `permessage-deflate` shall not be offered or accepted: without a negotiated compressor there is no decompression ratio to bound, and the relay compresses nothing.
- The client shall treat message size and parser work as bounded untrusted input.
- Heartbeat interval and timeout are stated by the relay in its `ready` message and shall be read from it rather than assumed; the adapter keeps configurable fallbacks for a relay that omits them.
- Exponential reconnect backoff with jitter shall be used, and shall reset only after a connection has remained established for a stability threshold — not merely after a successful handshake.

#### 7.4.1 Tunnelled transport

The client MAY reach the relay through an embedded user-space tunnel — an embedded Tailscale node — instead of the platform network stack. A tunnel supplies an already-connected stream socket for a host and port; every layer above the socket is unchanged.

- The tunnel MUST NOT be a device-wide VPN. Only the client's own connections to the relay travel through it.
- A tunnel MUST NOT expose a listening socket on the device's loopback interface, because any other application could then connect to it and reach the user's private network through this client.
- Cleartext through a tunnel MUST be refused unless it has been enabled explicitly; the loopback exception in §7.4 does not apply to a tunnelled connection. Deployments MAY enable it where the tunnel itself authenticates and encrypts the peer.
- When a tunnel is selected and is not connected, connection attempts MUST be refused and surfaced. The client MUST NOT fall back to a direct connection.
- Whether a session uses the tunnel MUST be fixed for that session's lifetime; changing it requires a new session.
- A tunnel MUST NOT upload diagnostics off the device, and its private key material MUST be stored under §12's rules for the device credential.
- The tunnel implementation MAY be absent from a build. Its absence MUST be reported as unavailability, never resolved by connecting directly.

## 8. Terminal emulation

### 8.1 Compatibility profile

The emulator shall target an `xterm-256color`-compatible environment and support at minimum:

- UTF-8 input and output.
- C0 controls: NUL, BEL, BS, HT, LF, VT, FF, CR, and ESC as applicable.
- Incrementally received ESC, CSI, OSC, and DCS sequences without assuming frame boundaries.
- Cursor movement, absolute positioning, save/restore, tab stops, and scrolling regions.
- Insert/delete character and line operations.
- Erase-in-display and erase-in-line operations.
- Primary and alternate screen buffers.
- DEC origin, auto-wrap, application cursor, application keypad, and cursor visibility modes.
- SGR text attributes: bold, faint, italic, underline, blink state, inverse, concealed, and strike-through where practical.
- Standard 16 colors, the 256-color palette, and 24-bit true color.
- Bracketed paste mode.
- Focus reporting and mouse-reporting modes if those inputs are enabled in the UI.
- OSC window title updates.
- Combining marks, East Asian wide characters, zero-width characters, and Unicode replacement behavior for invalid UTF-8.

Unknown, malformed, or unsupported escape sequences shall be ignored safely without crashing or placing the parser in an unrecoverable state. OSC clipboard operations and other sequences that access device resources shall be ignored unless explicitly permitted by a separately reviewed security policy.

The effective `TERM` value is controlled by the remote host/server. For the supported profile it should be `xterm-256color`, and the remote environment must provide matching terminfo data.

### 8.2 Screen model

- The active screen is a fixed grid of rows and columns.
- Each cell contains a Unicode grapheme representation or continuation marker, foreground/background color, text attributes, and width metadata.
- Primary-screen lines leaving the top of the viewport are appended to a bounded scrollback buffer.
- Alternate-screen content is not added to normal scrollback unless product requirements later state otherwise.
- The default scrollback limit shall be configurable, initially 10,000 logical lines, with a memory ceiling.
- Resizing shall preserve content according to a documented policy. Reflow is preferred for ordinary primary-screen text; alternate-screen applications shall preserve grid semantics.

### 8.3 Stream ownership

**PTY-stream mode** is the authoritative representation, and the only one. The relay sends raw ordered PTY output and performs no terminal emulation of its own: it does not require UTF-8, parse escape sequences, or normalize newlines in either direction. All terminal emulation happens on the Android client.

There is no state-mirror mode and no parsed-state snapshot. What the relay calls a snapshot is its bounded replay window — at most 1,500,000 bytes (decimal), the newest contiguous suffix of the stream — and resynchronizing means replaying those bytes.

Because local scrollback (§8.2) is far larger than that window, a `gap` means the earlier screen is unrecoverable: the client shall reset the emulator and rebuild from the available offset rather than splice new bytes onto stale state.

## 9. Keyboard and text input

### 9.1 IME integration

The Android host layer shall expose an editable input connection suitable for terminal use while keeping the terminal grid itself authoritative. It shall handle:

- Committed Unicode text.
- Composing text from predictive, dead-key, and multi-stage input methods.
- Deletion requests.
- Editor actions such as Enter.
- Soft-keyboard visibility and terminal focus.

Committed text shall be encoded as UTF-8. Composition updates shall not be sent remotely until committed, unless later protocol requirements explicitly support remote composition.

### 9.2 Physical and extra keys

- Printable hardware-key events shall respect Unicode text and active modifiers.
- Control combinations shall generate the conventional control bytes where defined, such as `Ctrl+C` → `0x03`.
- Navigation, editing, function, keypad, and modifier keys shall generate sequences appropriate to the terminal's current modes.
- `Alt` behavior shall be configurable if necessary; the initial behavior is to prefix compatible keys with ESC.
- Key-down repeats shall be supported. Duplicate key-down/text events produced by Android shall be filtered.
- The extra-key row shall support latched `Ctrl` and `Alt` for the next key and provide clear visual state.

### 9.3 Paste and input safety

- Paste shall preserve UTF-8 text and normalize line endings according to the server's PTY input contract.
- If bracketed paste is active, pasted content shall be wrapped in the appropriate begin/end control sequences.
- Large pastes shall be chunked with bounded buffering and visible cancellation where practical.
- Input generated while disconnected shall not be silently replayed. The policy is to reject it and show the disconnected state. The same applies to input that was sent but never acknowledged: it shall be surfaced, never resent on a new connection.
- Input shall not be sent at all when the attachment is read-only; the client shall say why rather than emit frames the relay would refuse.
- The app shall not log terminal input or output in release builds because it may contain passwords or sensitive data.

## 10. OpenGL ES rendering

### 10.1 Renderer requirements

- Create and manage an EGL surface tied to the Android window lifecycle.
- Render the terminal grid using batched geometry and one or more glyph-atlas textures.
- Render cell backgrounds, glyphs, decorations, selection, and cursor in deterministic layers.
- Support runtime viewport changes, display density, font-size changes, and device rotation.
- Use premultiplied-alpha blending where transparency is required.
- Rebuild GPU resources after EGL context loss without losing terminal state.
- Perform no network access or terminal parsing on the render thread.

The renderer shall redraw only when the terminal changes, the cursor blinks, an animation is active, or the surface changes. It should not continuously render at maximum refresh rate when the terminal is idle.

### 10.2 Text shaping and font behavior

- Ship or select a monospace font with broad Unicode coverage and a license suitable for redistribution.
- Rasterize glyphs into a dynamically managed atlas. Font rasterization/shaping may use an appropriate native library.
- Maintain terminal cell widths even when a fallback glyph is not intrinsically monospaced.
- Correctly place combining marks and double-width glyphs.
- Substitute a visible replacement glyph for missing characters.
- Font fallback and shaping must not block a frame on expensive unbounded work; newly required glyphs may be prepared asynchronously.

### 10.3 Grid sizing and resize requests

The usable terminal rectangle and chosen cell dimensions determine the number of columns and rows. A resize is sent to the server only when these integer dimensions change. Resize messages shall be debounced during rotation or interactive layout changes, while the final dimensions shall always be sent.

A resize is a **request, not a publication**. The publishing device owns the PTY and therefore its dimensions; the client sends `terminal.resize_request`, which the publisher may apply or ignore, and renders at whatever size the resulting `terminal.resize` reports. An operator may disable client-initiated resize independently of input, and a read-only attachment shall not send one at all.

When the relay reports no dimensions, the client renders at its own computed grid.

By default the client does not send resize requests at all; see §10.4.

### 10.4 Viewing a terminal larger than the screen

A mirrored terminal usually belongs to somebody working at the other end. Reshaping it to suit a phone reflows *their* session, so by default the client SHALL NOT request a resize: it renders the terminal at whatever size the publisher reports and provides a movable view over it.

- The client SHALL render the complete grid at its natural cell size, independent of the surface size.
- The client SHALL provide continuous zoom and pan over that rendering, and a gesture that returns to a default view. Fitting the full width is the default, because a terminal is read left to right and hidden columns cost more than small text.
- The view SHALL NOT be movable beyond the terminal's bounds. On an axis where the scaled terminal is smaller than the surface, it SHALL be centred.
- Zooming SHALL keep the point under the gesture's focus stationary, on each axis where the view can move.
- When the publisher's size changes, a view the user has not moved SHALL be refitted; a view the user has moved SHALL be left as it is, and only brought back within the new bounds.
- Coordinates for selection and mouse reporting SHALL be mapped through the inverse of the view transform. A point outside the terminal SHALL be reported as outside rather than clamped to the nearest cell.
- Requesting resizes MAY be re-enabled for a deployment whose publisher has no screen of its own to disturb. §10.3 then applies unchanged.

Rendering the grid once into an offscreen image and transforming that image is the expected implementation, so that a pan or a zoom costs no re-layout. An implementation SHALL degrade to drawing directly to the screen if it cannot allocate one.

## 11. Lifecycle, connectivity, and recovery

- Surface loss shall stop rendering but retain terminal state.
- Backgrounding shall follow a documented connection policy. The initial policy is to keep the connection only while allowed and useful, then detach cleanly or reconnect on resume.
- Authentication expiry shall trigger the server-defined refresh flow. A failed refresh returns the user to sign-in without exposing stale credentials.
- Network changes may trigger reconnection but shall not create concurrent attachments accidentally. The relay permits many simultaneous mirrors of one terminal and does not arbitrate between writers, so single-attachment discipline is entirely the client's responsibility.
- A connection attempt and all callbacks shall carry a generation identifier so late callbacks from an older connection cannot mutate the current session.
- Reopening a session shall restore authoritative server state, not rely solely on a locally cached screen.

## 12. Security and privacy

- The client's credential is a **`client`-role device key** of its own: an Ed25519 key pair generated on the device. The identity's root private key shall never reach the device, so losing the device costs one revocable credential rather than the identity.
- Store that key only through Android Keystore-backed secure storage. Where the platform cannot hold the key type directly, a Keystore-resident key shall seal it before it is written, and the sealing key shall never leave the Keystore.
- Keep access tokens in memory for the minimum practical duration and clear replaced sensitive buffers when feasible.
- Do not place secrets in URLs, analytics events, crash messages, or application logs.
- Validate all server-provided sizes, indices, colors, Unicode data, and control-sequence lengths before use.
- Bound terminal scrollback, pending network bytes, glyph atlases, and outbound input queues.
- Ignore remote attempts to read the Android clipboard, launch arbitrary URI schemes, write local files, or invoke Android intents by default.
- Screen-capture prevention shall be a user or deployment policy; if enabled, apply Android secure-window behavior to the terminal screen.
- Pairing screens display a public key and public identifiers only. There are no passwords to protect, but private key material, tokens, tickets, challenges and signatures shall never appear in logs, analytics, crash messages or backups.

## 13. Accessibility and usability

- Controls outside the terminal grid shall have Android accessibility labels and adequate touch targets.
- Connection state shall be conveyed by text or accessibility state, not color alone.
- Font size and terminal contrast shall be configurable.
- The terminal accessibility bridge should expose visible lines and selected text to screen readers without emitting an announcement for every high-frequency screen update.
- Gesture mappings shall not make ordinary vertical scrolling or text selection inaccessible.

## 14. Performance and resource targets

Targets are measured on a representative mid-range device agreed upon by the project:

- Display newly received terminal output within 50 ms at the 95th percentile, excluding network latency, while the app is foregrounded.
- Maintain 60 frames per second during ordinary scrolling and sustained output on a 60 Hz display.
- Process a burst of at least 1 MiB of typical terminal output without crashing, unbounded allocation, or UI-thread stalls longer than 100 ms.
- Keep steady-state parsing and rendering memory bounded by configured screen, scrollback, queue, and glyph-cache limits.
- Avoid measurable continuous GPU load when the displayed terminal is idle.
- Preserve input ordering under sustained output load.

Final numeric targets may be adjusted after the server protocol and reference devices are known, but tests shall continue to enforce bounded memory and non-blocking UI behavior.

## 15. Error handling

User-visible errors shall distinguish at least:

- No network connectivity.
- Authentication failure or expired session.
- Permission denied for a terminal session. The relay answers `404` rather than `403` for a resource the caller does not own, so "not found" and "not yours" are deliberately indistinguishable and shall be presented as one state.
- Remote terminal ended.
- Protocol incompatibility.
- Synchronization failure requiring a fresh replay.
- TLS or server identity failure.
- Unsupported client/server version.
- **Input refused** — the terminal did not opt in, the credential lacks the authority, or an operator disabled input. This is permanent for the attachment and the client shall present a read-only state.
- **Input temporarily undeliverable** — no publisher is connected, the publisher's queue is full, or a rate limit was hit. The attachment stays usable and the user may try again.

Detailed internal errors may be logged in debug builds after secrets and terminal contents are redacted. Fatal parser or renderer errors shall close the affected session safely rather than terminate the entire application where recovery is possible.

## 16. Testing strategy

### 16.1 Unit tests

- Incremental UTF-8 decoding across every possible input boundary.
- ANSI/VT parser state transitions, including truncated and malformed sequences.
- Screen operations, attributes, wide/combining characters, scroll regions, alternate screen, and resizing.
- Keyboard mapping under terminal modes and modifier combinations.
- WebSocket message ordering, duplication, revision gaps, and snapshot boundaries.
- Reconnect backoff and stale-callback generation handling.

Parser tests shall include recorded or generated output from Bash and common terminal programs such as `vim`, `less`, `top`, and `tmux`, subject to their licenses and test-environment availability.

### 16.2 Fuzz and robustness tests

- Fuzz the UTF-8 and escape-sequence parser with arbitrary chunk boundaries.
- Fuzz snapshot/delta decoding and all length fields.
- Exercise very long OSC/DCS sequences, resize storms, output floods, and decompression limits.
- Simulate EGL surface/context loss and repeated foreground/background transitions.

### 16.3 Integration and end-to-end tests

A fake server shall implement the normalized API behavior so client development does not depend on the production server. Tests shall cover:

- Registration/sign-in success and failure.
- Initial attach and snapshot synchronization.
- Interactive shell input and echoed output.
- Full-screen application behavior.
- Rotation and terminal resize.
- Disconnect during output, reconnect, gap detection, and state recovery.
- Disconnect during input, with verification that input is neither silently lost nor duplicated contrary to protocol guarantees.
- Expired credentials and successful/failed refresh.

Golden-image rendering tests should validate representative terminal grids on controlled fonts and GPU-independent reference rendering where possible.

## 17. Acceptance criteria for the first release

The release is acceptable when all of the following are demonstrated:

1. A user can authenticate using the supplied API and attach to an authorized terminal session.
2. A Bash prompt and continuous command output appear correctly, including UTF-8, ANSI colors, cursor motion, and alternate-screen applications.
3. Soft and hardware keyboards can enter ordinary text, control combinations, navigation keys, and function keys without reordering.
4. Rotation or font-size changes update the rendered grid and publish the correct remote terminal dimensions.
5. A forced network interruption results in a clear state and an authoritative, non-duplicated recovery after reconnect.
6. Malformed terminal and protocol inputs do not crash the app or cause unbounded memory growth.
7. OpenGL context loss recovers without losing the synchronized terminal model.
8. Release logs contain no authentication secrets, terminal output, or user keystrokes.
9. The performance targets in Section 14 are met on the agreed reference device or exceptions are documented and approved.
10. A terminal that does not accept input, or whose publisher is unreachable, attaches read-only and says so; typing into it is refused with a distinct, user-visible reason and nothing is sent.
11. Pairing produces a device credential whose private key never leaves the device, and revoking that device from the server ends the client's access.

## 18. Server API integration — resolved

Every item that previously blocked implementation freeze is answered by
`../server/spec.md` and `../server/RECONCILIATION.md`. The answers, in the order the
questions were originally asked:

1. **Registration and authentication.** `POST /v1/auth/challenges` → sign the returned length-prefixed `signing_input` → `POST /v1/identities` (register) or `POST /v1/auth/tokens` (authenticate). Challenges are single-use, expire within five minutes, and are consumed by the first verification attempt whether or not it succeeds. The private key is stored in Keystore-backed storage (§12).
2. **Refresh, revocation, logout, device registration.** There are no refresh tokens; re-running `authenticate_device` with the stored key is the refresh. Logout is discarding the local token. `DELETE /v1/devices/{id}` revokes server-side, immediately for new connections and within 30 seconds for live ones. Device registration needs both parties: the device signs, the identity authorises (§5.1).
3. **Discovery and attach authorization.** `GET /v1/terminals`, `GET /v1/terminals/{id}`, cursor-paginated. Visibility *is* attach authorization; a non-owner gets `404`.
4. **WebSocket construction.** `GET wss://<origin>/v1/terminals/{id}/mirror` with `Sec-WebSocket-Protocol: terminal-relay.mirror.v2` (v1 may be offered as a fallback and is read from the response), authenticated by `Authorization: Bearer` or a single-use path-bound ticket.
5. **Authoritative payload.** Raw PTY bytes, always (§8.3).
6. **Schemas, framing, compression, sizes, heartbeat.** JSON text frames for control, binary frames for payload. Mirror output frames are `0x01`, u64 start offset, payload; input frames are `0x02`, u64 client sequence, payload — note the publisher layout differs by inserting a terminal UUID. No compression. Sizes and heartbeat values are stated by the relay in `ready` (§7.1, §7.4).
7. **Sequence, acknowledgement, replay, deduplication, snapshots.** Monotonic 64-bit byte offsets, a bounded 1,500,000-byte replay window, `gap` below it, `offset_ahead` above it, and no gap or duplication at the replay-to-live boundary (§7.3, §8.3).
8. **Input acknowledgement.** Client sequences start at 1 per connection and advance only on acceptance, so the expected next value is always `accepted_through + 1`. A refused frame does not consume its sequence. Unacknowledged input is never resent (§7.3).
9. **Resize ownership.** The publishing device owns it; clients request (§10.3). The relay permits many concurrent mirrors and arbitrates nothing (§11).
10. **Encoding, `TERM`, locale, newlines.** The relay guarantees only that bytes arrive unchanged and in order. `TERM` is declared by the publisher and reported in the subscription reply; for the supported profile it is `xterm-256color` (§8.1). Locale and newline behaviour belong to the remote PTY.
11. **Session persistence.** A terminal survives a brief publisher disconnect and then closes with reason `publisher_disconnected`. When the shell exits, `terminal.closed` arrives after every accepted byte has been delivered. Closed terminals stay readable for a retention period, and terminal IDs are never reused.
12. **Minimum server version.** Any relay serving `/v1` and the `terminal-relay.mirror.v2` subprotocol. Breaking HTTP changes take a new base path; breaking WebSocket changes take a new subprotocol name. Unknown JSON fields are ignored; an unknown control-message type is fatal unless marked `"optional": true`.

No production protocol assumptions beyond this specification are embedded outside the API adapter.

## 19. Remaining open items

These are product or environment decisions, not protocol gaps:

1. **Reference device and measured performance.** §14's targets are stated against a device the project has not yet agreed on. The properties behind them are enforced by tests; the numbers need hardware.
2. **Pairing ergonomics.** §5.1 currently transfers a public key and two identifiers by hand. A QR-code carrier, or a server-issued short-lived pairing code, would improve it without any protocol change.
3. **Remote clipboard writes.** OSC 52 writes remain disabled pending the separately reviewed security policy §8.1 requires.
4. **Recorded terminal corpora.** §16.1 asks for recorded `vim`, `less`, `top` and `tmux` output; capturing it needs the reference environment.
