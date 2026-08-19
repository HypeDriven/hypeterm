# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**Hypeterm** (`com.hypedriven.hypeterm`): an Android client that displays a terminal
session hosted elsewhere and sends keystrokes back. It is a *mirror*, not a shell —
nothing is executed on the device.

`spec.md` still names the product "Terminal Mirror", and the C++ core still uses the
`tmirror` namespace and `tm/` include prefix. Those are internal; anything the user or
the platform sees — app name, package, native library, log tag — is Hypeterm.

`spec.md` (Draft 0.2) is normative. The server it talks to is specified in
`../server/spec.md`, and `../server/INTEGRATION.md` is the contract between the two;
read it before changing anything protocol-shaped. §19 lists what remains.

## Commands

The C++ core and its entire test suite build with CMake, a C++17 compiler and OpenSSL —
no Android SDK involved. The app build is below.

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j
ctest --test-dir build --output-on-failure       # unit + integration + fuzz corpora

./build/tests/tm_unit_tests --filter=Screen      # one suite
./build/tests/tm_unit_tests --list               # every test name
TM_TEST_LOG=1 ./build/tests/tm_integration_tests # with client logs

# Sanitizers: how the fuzz targets are meant to run.
cmake -S . -B build-asan -DCMAKE_BUILD_TYPE=Debug -DTM_ENABLE_ASAN=ON
cmake --build build-asan -j && ctest --test-dir build-asan

./build/tests/fuzz_terminal --iterations=200000 --seed=1   # longer local fuzzing
./build/tests/fuzz_terminal crash-input.bin                # replay a reproducer
```

The integration tests start `tools/fake_relay/relay.py` as a child process; they skip
themselves, loudly, when `python3` is unavailable.

The Android app builds and runs. The toolchain lives at `~/Android/Sdk` (platform 35,
build-tools 35.0.0, NDK 27.0.12077973, CMake 3.22.1) with Gradle at
`~/tools/gradle-8.11.1`; `local.properties` and `gradle.properties` already point at
them.

```bash
export JAVA_HOME=$HOME/.local/opt/jdk-21    # wherever your JDK 21 is
tools/build-openssl-android.sh          # once; static OpenSSL 3, 16 KB aligned
tools/build-tsnet-android.sh            # optional; embedded Tailscale, ~21 MB per ABI
./gradlew :app:assembleDebug
```

The Tailscale node (`tsnet/`) is a separate Go library, `dlopen`'d at runtime, so both
the app and the tests work without it. It needs Go 1.24+ at `~/tools/go1.24/bin/go`
(the system Go is too old for Tailscale; the toolchain fetches a newer one on demand).
`tools/build-tsnet-host.sh` builds the same library for this machine so
`test_tailscale.cpp` can exercise the real C API.

WSL2 owns no USB device, so installing goes through the Windows `adb` with a Windows
path — see `docs/android-build.md`, which also documents running the whole app against
the fake relay over `adb reverse`. It has been exercised on a Galaxy S24 Ultra
(Android 16): pairing, attach, rendering, input, resize, rotation, reconnect, surface
loss. What remains unmeasured is spec §14's performance numbers.

## Layout

| Path | Responsibility |
| --- | --- |
| `core/include/tm/`, `core/src/` | The whole client: emulator, protocol, networking, rendering, controller |
| `core/src/term/` | UTF-8, the VT parser, screen, scrollback, reflow — the largest correctness surface |
| `core/src/api/` | The only files that know the relay's wire format (spec §7.1) |
| `core/src/app/controller.cpp` | Session lifecycle, reconnect, generations, command routing |
| `core/src/render/` | Palette, glyph atlas, frame builder, view transform, CPU reference renderer, GL ES backend |
| `app/src/main/cpp/` | JNI bridge, render thread, `android.graphics` rasterizer |
| `app/src/main/kotlin/` | Platform layer only: activities, IME, Keystore, clipboard, connectivity |
| `tests/` | `unit/`, `integration/`, `fuzz/`, plus a small in-tree test framework |
| `tsnet/` | Go: the embedded Tailscale node behind a six-function C API (spec §7.4.1) |
| `tools/fake_relay/` | Fake relay with real Ed25519 and every failure path under `/_test/` |
| `docs/` | architecture, protocol-integration, resize-policy, security, testing, acceptance, android-build |

## Conventions this codebase holds to

- **The namespace is `tmirror`, not `tm`.** `tm` collides with C's `struct tm` from
  `<time.h>` and will not compile. Include paths still use `tm/`.
- **Nothing above `core/src/api/` knows the wire format.** Protocol changes stop at the
  adapter (spec §7.1, §18 closing line). If a relay field name appears in `term/`,
  `render/` or `app/`, that is a bug.
- **A device holds one publisher connection** (relay spec §6.1). A second `run` on the
  same machine supersedes the first, which must then *stop* rather than reconnect —
  two processes trading a device between them re-open a terminal every round. Losing
  the mirror never takes the shell with it.
- **Pairing needs both parties.** The owner's token authorises, the device's key
  signs. The client's original two-field flow only did the first half and works solely
  against the fake relay; `CompletePairingWithCode` is the one that works in
  production. The code format is shared with `publisher/src/pairing.rs`, and a fixed
  vector in both test suites is what keeps them agreeing.
- **The client never resizes the remote terminal.** `follow_remote_size` is on by
  default: the grid is whatever the publisher runs at, drawn once into an offscreen
  texture, and the user zooms and pans over it (`render::Viewport`, spec §10.4).
  Reshaping a session somebody is working in is not the phone's decision to make.
- **Terminal logic never lives in Kotlin.** The JVM layer exists for IME composition,
  the Keystore, the clipboard, accessibility, connectivity and font rasterization — the
  platform APIs with no usable native equivalent (spec §6.1). Anything else belongs in
  `core/`.
- **Respect the thread that owns each thing.** The network thread owns the emulator and
  the sockets; the render thread owns EGL and every GL call; the UI thread owns platform
  callbacks. The only things that cross are an ordered bounded command queue one way and
  immutable snapshots the other (spec §6.2).
- **Never destroy a session from inside its own callback.** `MirrorSession` callbacks run
  inside its read loop; tearing it down there is a use-after-free. Set
  `pending_disconnect_` and act after `Pump` returns.
- **Every bound is real.** Scrollback lines *and* bytes, JSON depth and size, CSI
  parameters, OSC/DCS payloads, the glyph atlas, the command queue. A full queue is
  reported, never silently drained (spec §6.2, §9.3, §12).
- **An offset is a byte count**, not a message counter, and it is parsed exactly from its
  literal so 64-bit values never pass through a double.
- **Unacknowledged input is never resent.** After an ambiguous disconnect, tell the user;
  do not guess (relay integration §7).
- **Nothing sensitive reaches a log.** `TM_LOG_PAYLOAD` compiles away outside debug
  builds; `Log::Redacted()`, `Log::ByteCount()` and `SanitizeForMessage` are the only
  permitted representations of anything that came off the wire.
- **TLS verification has no off switch.** Adding trust anchors is supported because
  Android requires it; disabling verification is not, anywhere (spec §7.4).
- **Comments explain why, not what**, and cite the spec section when the reason lives
  there (`spec §8.2`, `relay spec §6.3`).
- **Tests assert specified behaviour**, are named for what would break, and run without a
  device.

## Things that bite

- `"\x1bc"` in C++ is the single character `0x1bc`, not ESC followed by `c`. Write
  `"\x1b" "c"`.
- The mirror output frame is `0x01 | u64 offset | payload`. The *publisher* frame inserts
  a 16-byte terminal UUID. Crossing them misparses everything.
- `accepts_input` is the publisher's opt-in; `input_available` is whether *this*
  subscription may type right now. Only the second one decides.
- CMake `file(GLOB)` needs `CONFIGURE_DEPENDS`, or a new test file is silently not built.
- The fake relay's signing input binds the origin it was started with; the client
  tolerates a mismatch but the test harness assumes they agree.
- Android 15+ rejects native libraries that are not 16 KB-page aligned. The APK carries
  `libhypeterm.so` (static OpenSSL, `ANDROID_STL=c++_static`,
  `-Wl,-z,max-page-size=16384`) and, when it was built, `libhypeterm_tsnet.so` (the Go
  linker needs `-extldflags=-Wl,-z,max-page-size=16384`). Check both with
  `llvm-readelf -l` after changing any of it.
- `tsnet` uploads node diagnostics to `tailnode.log.tailscale.io` by default, and its
  `Logf` field does not stop it — that is logtail, a separate path.
  `envknob.SetNoLogsNoSupport()` in `tsnet/bridge.go`'s `init()` is what turns it off.
- Three things break Go/Tailscale inside an Android app, and all three are fixed in
  `tsnet/`. Each one fails differently, so the symptom is worth recognising:
  1. `net.Interfaces()` is denied (`route ip+net: netlinkrib: permission denied`) —
     Android refuses `RTM_GETLINK`. `tsnet/interfaces.go` answers with `getifaddrs(3)`
     through `netmon.RegisterInterfaceGetter`.
  2. There is no `HOME`, no `TMPDIR`, and the working directory is `/`, so Tailscale
     finds nowhere to keep log state and **panics**, killing the process with a bare
     `SIGABRT`. `hypeterm_tsnet_start` sets `XDG_CACHE_HOME` and `TMPDIR` to
     app-private directories first.
  3. **The Go runtime does not see a `setenv` made from C.** Setting those variables in
     C++ before `dlopen` has no effect; it has to be `os.Setenv` from Go.
- `tsnet.Server.Start()` never begins a login, so a node with no auth key sits in
  `NeedsLogin` forever and no URL is ever issued. `watchStatus` calls
  `StartLoginInteractive`; the URL then takes 20–30 seconds to arrive.
- A Go panic writes to stderr, which Android discards — the crash arrives as a bare
  SIGABRT with no message and a one-frame backtrace. `StartStderrRelay` in
  `jni_bridge.cpp` forwards stderr to logcat in debug builds; without it these are
  close to undebuggable.
- A tunnel must never expose a loopback listener on Android: any app can connect to
  another app's `127.0.0.1` port. The dial seam hands over a socketpair descriptor
  instead (`net::Dialer`).
- `targetSdk = 35` means edge-to-edge. Any new screen needs
  `applySystemWindowPadding()` or it draws under the status bar and the keyboard.
- The native controller is constructed before the user has entered a relay URL. Call
  `NativeBridge.setServerUrl` before `start()`, never after.
