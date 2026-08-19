# Acceptance criteria

Spec §17 lists what must be demonstrated for the first release. This maps each
criterion to where it is demonstrated, and says plainly which ones are not.

"On device" below means a Galaxy S24 Ultra (SM-S928B, Android 16, arm64-v8a) running
the debug APK against `tools/fake_relay/relay.py` reached through `adb reverse`.

| # | Criterion | Where it is demonstrated | State |
| --- | --- | --- | --- |
| 1 | Authenticate and attach to an authorized terminal | `Integration.AttachRendersReplayedAndLiveOutput`, `Crypto.*`; **on device**: key generated in-app, registered, challenge signed, token issued, mirror attached | Demonstrated, host and device |
| 2 | Bash prompt and continuous output: UTF-8, colours, cursor motion, alternate screen | `Integration.AnsiColoursCursorMotionAndAlternateScreen`, `Screen.*`, `Emulator.*`, `Utf8.*`; **on device**: colours, bold/underline/inverse, wide CJK, combining marks and emoji all rendered | Demonstrated, host and device |
| 3 | Soft and hardware keyboards: text, control combinations, navigation, function keys, no reordering | `Integration.TypedInputReachesTheTerminalInOrder`, `Input.*`; **on device**: typing through the soft keyboard arrived as frames with client sequences 1..7, in order, exactly once | Demonstrated, host and device |
| 4 | Rotation or font-size change updates the grid and publishes the right dimensions | `Integration.ResizeIsRequestedAndTheServerSizeWins`, `Screen.Resize*`; **on device**: portrait 55×24 → landscape 120×17 → back, each rotation producing one request the publisher applied | Demonstrated, host and device |
| 5 | A forced interruption gives a clear state and a non-duplicated recovery | `Integration.ReconnectResumesWithoutDuplicatingOutput`, `…EvictedOffsetProducesAGap…`, `…OffsetAheadResubscribes…`; **on device**: relay-forced disconnect, reconnect and resume with no duplication | Demonstrated, host and device |
| 6 | Malformed terminal and protocol input neither crashes nor grows without bound | Four fuzz targets under ASan/UBSan, 50 000 cases each; `Parser.*`, `Protocol.Malformed*` | Demonstrated |
| 7 | OpenGL context loss recovers without losing the terminal model | **On device**: backgrounded and restored — surface destroyed, GPU resources rebuilt, terminal model intact including output that arrived while there was no surface | Partially demonstrated: surface loss yes, a forced `EGL_CONTEXT_LOST` not yet |
| 8 | Release logs contain no secrets, terminal output or keystrokes | `TM_LOG_PAYLOAD` compiles away outside debug builds; every call site audited; **on device**: logcat during a full session shows only endpoints, status codes and byte counts | Demonstrated by construction, audit and inspection |
| 9 | Performance targets on the agreed reference device | `Performance.*` enforces the properties (linear parsing, bounded memory, cheap snapshots); `Integration.LargeOutputBurstIsAbsorbed` covers the 1 MiB burst | **Not demonstrated** — no agreed device, no profiling run |
| 10 | A read-only terminal attaches read-only, says so, and sends nothing | `Integration.ReadOnlyAttachmentRefusesInputAndSaysSo`, `…InputRefusedWhenNoPublisherIsConnected`, `Protocol.ReadOnlySubscriptionRefusesInputLocally` | Demonstrated |
| 11 | Pairing produces a device credential whose private key never leaves the device; revoking it ends access | `Crypto.SigningInputVerification*`, `Credentials.*`; **on device**: only the public key and a signature crossed the wire, and the Keystore-sealed credential survived reinstall and cold start | Demonstrated; revocation timing needs the production relay |

## Beyond §17: the embedded Tailscale tunnel

Not an acceptance criterion — it postdates the list — but held to the same standard.

| Property | Where it is demonstrated | State |
| --- | --- | --- |
| The whole client stack works through a supplied descriptor rather than `connect()` | `Tunnel.*`, including a complete controller session against a host name that cannot resolve | Demonstrated |
| Cleartext through a tunnel is refused unless enabled explicitly | `Tunnel.CleartextThroughATunnelIsRefusedUnlessEnabled`; **on device**: the app refused to reach its `http://` relay through the tunnel | Demonstrated, host and device |
| A tunnel that is not connected refuses to dial, with no fall back to a direct connection | `Tunnel.ConnectionsAreRefusedWhileTheTunnelIsNotReady`, `Tailscale.AStartedNodeCarriesNoTrafficUntilItIsAuthorised` | Demonstrated |
| A build without the library degrades to "unavailable" | `Tailscale.AnAbsentLibraryIsReportedRatherThanBypassed` | Demonstrated |
| No diagnostics are uploaded to Tailscale | `Tailscale.*` assert the node's own `no_log_upload`; **on device**: the node log records `TS_NO_LOGS_NO_SUPPORT="true"` | Demonstrated, host and device |
| The node starts on a real phone and reaches the coordination server | **On device** (S24 Ultra, Android 16): interfaces enumerated, node started, `NeedsLogin`, login URL issued | Demonstrated |
| A terminal mirrored over a real tailnet | — | **Not demonstrated** — needs a tailnet and a relay inside it |

## What is left

**Criterion 9** needs hardware the project has agreed on and a profiling run against
§14's numbers: 50 ms p95 output-to-display, 60 fps while scrolling, no UI-thread stall
over 100 ms during a 1 MiB burst. The properties behind those numbers are enforced by
`test_performance.cpp`, but properties are not measurements.

**Criterion 7** needs an instrumentation test that forces `EGL_CONTEXT_LOST` rather
than only destroying the surface. The recovery path is written for it —
`EglSurface::SwapBuffers` detects it, `GlRenderer::OnContextLost` forgets the handles
without calling GL, the atlas is cleared and a redraw is requested — and the
surface-loss half of it has now run on real hardware, but the context-loss half has
not.

Everything else runs today, either with `ctest --test-dir build` or on the device using
the procedure in `docs/android-build.md`.
