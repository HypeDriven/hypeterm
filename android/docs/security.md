# Security and privacy

Implements spec §12, with the transport rules from §7.4 and the logging rules from
§9.3 and §15.

## The credential

The client's credential is a **`client`-role device key**: an Ed25519 key pair
generated on the phone. The identity's root private key never reaches the device
(relay reconciliation §1.2), so a stolen or compromised phone costs the owner one
revocable device, not their identity.

Revocation is server-side (`DELETE /v1/devices/{id}`) and takes effect immediately for
new connections and within thirty seconds for live ones. "Signing out" in the app only
discards the local copy; it does not end access from the server's side, and the UI
should say so.

### How the private key is stored

The minimum supported platform has no hardware Ed25519, so the seed cannot live inside
the Keystore directly. Instead `KeystoreSecureStore` generates an AES-256-GCM key *in*
the Keystore — StrongBox when the device has it — which never leaves it, and uses that
key to seal the Ed25519 seed before it is written to shared preferences. Extracting the
seed then requires code execution on the device with the app's identity.

The key is deliberately **not** bound to user authentication: the client reconnects in
the background and a locked screen must not destroy the session. A deployment that
wants a stricter posture changes that one `KeyGenParameterSpec`.

`DeviceCredentials` zeroes the seed on destruction, and the serialized form is zeroed
after it is handed to storage.

## What the node reports about itself

The embedded node's status carries a login URL while it waits to be authorised, and
whoever holds that URL can join a machine to the tailnet. It must not reach a log
(spec §9.3, §12, §15). Only the state name and the node's own name are logged; the URL
goes through `TM_LOG_PAYLOAD`, which compiles away outside a debug build.

The way that gets broken is logging the whole status document because it is convenient
— which is exactly what a diagnostic line on the start-failure path did until
`Tailscale.NothingTheNodeSaysAboutItselfReachesTheLog` was written. That test installs
a log sink, drives the dialer through its status and failure paths, and fails if a
login URL, an `auth_url` key, or the document's own shape appears in anything logged.

## Pairing

Registering a device takes both parties (relay spec §5.2): the request is authorised by
an *identity* token, and the challenge is signed by the *device* key. Neither half is
sufficient, which is what stops anyone enrolling a key they do not hold.

A phone cannot hold the identity key — that is the whole point of the `client` role —
so the owner's half is delegated as a **pairing code**: `HT1.<base64url(json)>`
carrying the relay URL, the identity ID and a short-lived `devices:write` identity
token. The phone then requests its own `register_device` challenge, verifies that the
challenge binds *its* key to *that* identity, signs it, and registers itself. Its
private key never leaves it and the owner's machine never sees it.

The code is a credential while it lives:

- It is minted on demand by `hypeterm-publish pair-code` and expires with the token,
  in minutes.
- It is never written to disk on the device; only the resulting device credential is
  kept, sealed as above.
- The device refuses to sign a challenge that names an identity other than the one in
  the code, so a tampered code cannot obtain a signature binding the phone elsewhere
  (`Pairing.ACodeForAnotherIdentityIsRefusedBeforeSigning`).

The older two-field flow — type an identity ID and a device ID — remains in the app for
the development relay, which lets an owner vouch for a key on its own. It cannot work
against the real relay, and that is correct: the real relay demands the signature.

## Tokens

Access tokens live in memory only, last at most fifteen minutes, and are replaced by
re-authenticating with the device key. A replaced token's buffer is zeroed
(`SecureZero`). Tokens travel in the `Authorization` header and never in a URL; the
single-use WebSocket ticket exists for proxies that strip that header.

## Transport

`net::TlsTransport` has no "insecure", "skip verification" or "trust all" option
anywhere in its interface. Verification is `SSL_VERIFY_PEER`, hostname checking goes
through `X509_VERIFY_PARAM_set1_host`, and TLS 1.2 is the floor.

Android's trust store is not readable by OpenSSL, so the host layer exports its anchors
from `AndroidCAStore` as PEM and passes them in `TlsConfig::trust_anchors_pem`. That
*adds* anchors; a deployment that pins certificates supplies its own list here instead.
Cleartext is reachable only for a loopback host, which is how the integration tests run
and where nothing can leave the device.

## The embedded Tailscale node

The app can reach a relay that is not exposed to the internet by joining the user's
tailnet itself. It is *not* a VPN: `tsnet` runs a WireGuard node in user space over a
netstack, so there is no `VpnService`, no consent dialog, and nothing on the device is
rerouted — only this app's own connections to the relay.

The seam is descriptor handoff. `net::Dialer::DialFd` returns a connected socket and
everything above it (`TcpTransport::Adopt`, TLS, HTTP, WebSocket, the controller) is
unchanged. The Go side dials the tailnet and pumps bytes between that connection and
one end of an `AF_UNIX` socketpair.

A loopback listener would have been simpler and was rejected: on Android any app may
connect to another app's `127.0.0.1` listener, which would have made Hypeterm an open
proxy into the user's tailnet for every other app on the phone. A socketpair descriptor
is reachable only by the process holding it.

Rules the tunnel adds:

- **Cleartext through a tunnel is refused unless it is switched on explicitly.** The
  loopback exception does *not* apply: the descriptor is not a loopback address, and a
  tunnelled cleartext connection would leave the device unprotected up to the tunnel
  entrance. Inside a tailnet, plain HTTP is defensible — WireGuard already
  authenticates and encrypts the peer, and a tailnet address has no public certificate
  — so the option exists, off by default (`allow_cleartext_over_tunnel`).
- **A tunnel that is not connected refuses to dial.** There is no fallback to a direct
  connection: if the user asked for the tunnel, traffic either goes through it or does
  not go.
- **A build without the Tailscale library degrades to "unavailable"**, never to a
  direct connection. The library is loaded with `dlopen`, so its absence is a runtime
  condition, not a link error.
- **No diagnostics are uploaded.** `tsnet` otherwise ships node logs — peer names,
  addresses, link state — to `tailnode.log.tailscale.io`. `envknob.SetNoLogsNoSupport()`
  in the bridge's `init()` turns that off before anything starts, and the node reports
  the fact back in its status as `no_log_upload` so the client asserts it rather than
  trusting it.
- **The node key lives in app-private storage** (`filesDir/tailscale`, mode 0700). The
  auth key is a credential: it is sealed by the same Keystore-backed store as the device
  key, never written to the settings file and never logged.

One consequence of running the node in-process: Android denies applications the netlink
call Go uses to list interfaces, and gives them no writable temporary or cache
directory. Both are worked around inside `tsnet/` — with `getifaddrs(3)` and with
app-private directories — rather than by relaxing anything. Neither workaround widens
what the app can reach.

Turning the tunnel on or off rebuilds the native session. Whether connections take the
tunnel is fixed when the controller is constructed, deliberately — a live session must
not change the path its traffic takes underneath itself.

## Logging

Spec §9.3, §12, §15 and acceptance criterion 8 all say the same thing: no terminal
output, terminal input, keystrokes, tokens, tickets, challenges or signatures in
release logs. Two mechanisms enforce it:

- `TM_LOG_PAYLOAD` expands to nothing outside a debug build, so a payload log line
  cannot exist in a release binary even if someone writes one.
- The only permitted descriptions of sensitive values are `Log::Redacted()` and
  `Log::ByteCount()`, and every message that quotes server text passes through
  `SanitizeForMessage`, which strips control characters and truncates.

The relay applies the mirror-image rule on its side, and terminal input is never
persisted anywhere on either end.

## Untrusted input

Everything from the network is untrusted, and so is everything from the JVM:

- JSON parsing bounds total size, nesting depth and element count.
- Control-message and OSC/DCS payloads are bounded; an oversized one is truncated but
  still scanned to its terminator so the parser cannot be wedged.
- CSI parameters have fixed capacity and saturating values.
- Server-provided columns and rows are clamped before they reach the grid.
- Offsets are parsed exactly from their literal, so a 64-bit value never passes through
  a double.
- The glyph bitmap returned by the JVM rasterizer is size-checked before it is read.

## What the remote terminal may not do

- **Read the clipboard.** OSC 52 read requests (`?`) are never answered, in any
  configuration.
- **Write the clipboard**, unless a deployment sets `allow_clipboard_write`. Off by
  default (spec §8.1).
- **Resize, move or query the window.** `CSI t` window manipulation is ignored.
- **Change the palette or query colours.** OSC 4/10/11 and friends are ignored.
- **Launch anything.** There is no URI, intent or file-writing path in the emulator at
  all.

## Screen capture

`FLAG_SECURE` is applied to the terminal screen when `secure_window` is set. It is a
deployment policy, not a default, because it also blocks legitimate screenshots and
screen sharing.

## Backups

`data_extraction_rules.xml` excludes everything from cloud backup and device transfer.
The sealed credential would be useless on another device anyway, and the resume cursors
and cached metadata describe someone's sessions.
