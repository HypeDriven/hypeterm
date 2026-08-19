# Testing

Implements spec §16. Everything here runs on a developer machine with CMake, a C++17
compiler and OpenSSL — no Android SDK, no device, no emulator.

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug && cmake --build build -j
ctest --test-dir build --output-on-failure

./build/tests/tm_unit_tests --filter=Screen      # one suite
./build/tests/tm_unit_tests --list               # every test name
TM_TEST_LOG=1 ./build/tests/tm_integration_tests # with client logs
```

## Unit tests (spec §16.1)

`tests/unit/`, one file per area.

| File | Covers |
| --- | --- |
| `test_utf8.cpp` | Incremental UTF-8 decoding at **every** chunk boundary, overlongs, surrogates, maximal-subpart replacement, character widths |
| `test_parser.cpp` | Parser state transitions, truncated and malformed sequences, bounded OSC/DCS, parameter clamping, chunk-boundary equivalence |
| `test_screen.cpp` | Deferred wrap, wide and combining characters, erase/insert/delete, scroll regions, tab stops, alternate screen, scrollback bounds, reflow |
| `test_emulator.cpp` | SGR including colon sub-parameters, mode tracking, cursor styles, titles, device reports, reset, snapshot sharing |
| `test_input.cpp` | Key mapping under every terminal mode and modifier, keypad, function keys, paste normalisation and chunking, the duplicate-event filter, mouse encodings, and how a latched modifier divides a delivery of typed text |
| `test_protocol.cpp` | Ordering, duplication, overlap, gaps, subscription boundaries, malformed frames, error mapping, input sequencing |
| `test_crypto.cpp` | Fingerprints, length-prefixed encodings, signing-input verification and its refusals, credential round-trip |
| `test_util.cpp` | JSON limits and exactness, base64url, URL parsing, bounded queues, backoff |
| `test_render.cpp` | Grid computation, palette resolution, atlas bounds and lazy rasterization, frame layering, golden-image determinism |
| `test_app.cpp` | Session view state, whether the session is at the latest output, selection extraction, preference persistence and its bounds |
| `test_performance.cpp` | The properties behind spec §14: linear parsing, cheap snapshots, no idle work |

Two conventions worth keeping: tests assert *specified* behaviour rather than
implementation details, and each one is named for what would break.

## Integration tests (spec §16.3)

`tests/integration/` drives the whole client stack — TCP, TLS-capable HTTP, WebSocket,
the API adapter, the emulator, the controller — against `tools/fake_relay/relay.py`.
Nothing is stubbed, because the failures worth testing live in the seams.

The fake relay implements the normalized behaviour of `../server/spec.md`, including
real Ed25519 verification, and exposes its failure paths under `/_test/`:

| Test | Spec item |
| --- | --- |
| `AttachRendersReplayedAndLiveOutput` | §16.3 initial attach and snapshot synchronisation |
| `AnsiColoursCursorMotionAndAlternateScreen` | §17.2 full-screen application behaviour |
| `TypedInputReachesTheTerminalInOrder`, `ControlKeysAndFunctionKeysArrive` | §16.3 interactive input, §17.3 |
| `PasteIsBracketedWhenTheRemoteAsksForIt` | §9.3 |
| `ReadOnlyAttachmentRefusesInputAndSaysSo`, `InputRefusedWhenNoPublisherIsConnected` | relay §4.5, §6.3 |
| `ResizeIsRequestedAndTheServerSizeWins` | §16.3 rotation and resize, §17.4 |
| `ReconnectResumesWithoutDuplicatingOutput` | §16.3 disconnect during output, §17.5 |
| `EvictedOffsetProducesAGapAndRebuildsTheScreen` | §7.3 gap detection |
| `OffsetAheadResubscribesFromTheRetainedWindow` | relay §6.2 after a restart |
| `TerminalClosedIsSurfacedAndStopsReconnecting` | §15 remote terminal ended |
| `UnacknowledgedInputIsSurfacedAndNeverReplayed`, `InputIsRejectedWhileDisconnected` | §7.3, §9.3 |
| `ExpiredTokenIsRefreshedWithoutUserInteraction` | §16.3 expired credentials |
| `RejectedUpgradeIsReportedAsAnAuthFailure` | §15 authentication failure |
| `VersionOneRelayAttachesReadOnly` | relay §6 version negotiation |
| `LargeOutputBurstIsAbsorbed` | §14 the 1 MiB burst |
| `Tls.TrustedCertificateConnectsAndUntrustedDoesNot`, `Tls.HostnameMismatchIsRejected`, `Tls.CleartextIsRefusedToANonLoopbackHost` | §7.4 TLS validation, §15 TLS failure state |
| `Pairing.*` | relay §5.2 device registration, both halves |
| `View.*` (unit) | §10.4 zoom and pan over a terminal larger than the screen, and §5.2 following the newest output: which row to show, and which gestures stop it |
| `Integration.TheRemoteSizeIsNeverChangedByDefault`, `…ResizeIsRequestedWhenTheClientIsAllowedToAsk` | §10.3, §10.4 both resize policies |
| `Tunnel.*` | §7.4.1 tunnelled transport |
| `Tailscale.*` | §7.4.1 the embedded node |

They skip themselves, loudly, when `python3` is unavailable.

### The tunnel

Tailscale itself needs a tailnet, which no test can assume, so the coverage is split at
the seam:

- `test_tunnel.cpp` substitutes `tests/loopback_dialer.h` for the tunnel and drives the
  *whole* stack through it — HTTP, WebSocket, the API adapter, a complete controller
  session. Like a real tunnel, the stand-in resolves names itself, so the tests use a
  host that cannot resolve (`relay.internal.invalid`); if any connection escaped the
  dialer it would fail rather than quietly succeed. Descriptor adoption, the
  cleartext-through-a-tunnel refusal and the not-ready refusal are covered here.
- `test_tailscale.cpp` drives the real Go library through its real C API: it loads,
  starts a node against an unreachable coordination server, decodes its status, refuses
  to dial until connected, and tears down. It also asserts `no_log_upload`, so the
  diagnostics opt-out is a checked property rather than a comment. It needs
  `tools/build-tsnet-host.sh` and skips without it.

What remains untested here is only Tailscale's own networking, which needs a device and
a tailnet.

## Fuzzing (spec §16.2)

`tests/fuzz/`, four targets, described in `tests/fuzz/README.md`. Each is a standard
`LLVMFuzzerTestOneInput`, so the same source runs under a deterministic corpus in CI
and under libFuzzer locally. Run them against the sanitizer build:

```bash
cmake -S . -B build-asan -DCMAKE_BUILD_TYPE=Debug -DTM_ENABLE_ASAN=ON
cmake --build build-asan -j && ctest --test-dir build-asan
```

Their assertions are specification bounds — scrollback limits, snapshot well-formedness,
offset contiguity, JSON round-tripping — so a failure is a spec violation.

## Golden-image rendering

`render::ReferenceRenderer` draws a `RenderFrame` on the CPU, in the same layer order
as the GL backend, using a deterministic built-in font. `test_render.cpp` compares
content fingerprints, so a layout regression fails on a developer machine without a GPU
or an installed font (spec §16.3).

## What is not covered here

- **The Kotlin layer and the GL backend are not built by this suite.** No SDK or NDK is
  present in the development environment, so `app/` and `core/src/render/gl_renderer.cpp`
  compile only in the Gradle build. `docs/android-build.md` says what to run once a
  toolchain exists, and instrumentation tests for the IME, surface lifecycle and EGL
  context loss belong there.
- **Device performance numbers.** Spec §14's targets are stated against an agreed
  reference device. `test_performance.cpp` enforces the shape of those properties, not
  the numbers; the numbers need a device and a profiler.
- **Recorded output from `vim`, `less`, `top` and `tmux`.** Spec §16.1 asks for these.
  The emulator tests cover the mechanisms those programs use — alternate screen, scroll
  regions, insert/delete, DEC line drawing, SGR — but recorded transcripts should be
  added when the reference environment exists to record them.
