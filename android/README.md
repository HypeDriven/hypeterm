# Hypeterm

An Android client that mirrors a terminal session hosted elsewhere. It displays the
remote terminal and sends keystrokes back; it never runs a shell on the device.

The app ships as **Hypeterm** (`com.hypedriven.hypeterm`). `spec.md` is the normative
specification and still calls the product "Terminal Mirror" — that is the specification
document's name for it, not the shipped one. The server it talks to is specified in
`../server/spec.md`, and `../server/INTEGRATION.md` defines how the two
specifications fit together.

## Layout

| Path | What lives there |
| --- | --- |
| `core/` | The C++17 core: terminal emulation, protocol, networking, rendering, controller |
| `app/` | The Android application: Gradle module, Kotlin platform layer, JNI bridge |
| `tests/` | Host unit tests, integration tests against the fake relay, fuzz targets |
| `tools/fake_relay/` | A fake relay implementing the normalized server behaviour and its failure paths |
| `docs/` | Architecture, protocol integration, resize policy, security, testing, acceptance, Android build |

The division follows spec §6.1: terminal logic, networking orchestration, protocol
translation and rendering are all in `core/`. The Kotlin in `app/` exists only for
platform APIs with no usable native equivalent — IME composition, the Keystore, the
clipboard, accessibility, connectivity and font rasterization.

## Building and testing on a development machine

The core builds and its whole test suite runs with nothing but CMake, a C++17 compiler
and OpenSSL. No Android SDK is involved.

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build -j
ctest --test-dir build --output-on-failure
```

That runs the unit tests, the fuzz targets over a deterministic corpus, and the
end-to-end tests against `tools/fake_relay/relay.py` (which needs `python3`; the
integration tests skip themselves without it).

For the sanitizer build, which is how the fuzz targets are meant to be run:

```bash
cmake -S . -B build-asan -DCMAKE_BUILD_TYPE=Debug -DTM_ENABLE_ASAN=ON
cmake --build build-asan -j && ctest --test-dir build-asan
```

## Building the Android application

```bash
tools/build-openssl-android.sh          # once: static OpenSSL 3 per ABI
./gradlew :app:assembleDebug
./gradlew :app:installDebug
```

`docs/android-build.md` covers the SDK/NDK versions, the 16 KB page-size requirement,
installing from WSL2, and how to run the app against the fake relay over `adb reverse`
without any production server.

## Where to start reading

- `docs/acceptance.md` — every criterion in spec §17, where it is demonstrated, and what
  still needs a reference device.
- `docs/architecture.md` — components, threads, and what crosses which boundary.
- `docs/protocol-integration.md` — how the client speaks to the relay, and why the
  offset bookkeeping looks the way it does.
- `core/include/tm/term/emulator.h` — the terminal state machine.
- `core/include/tm/app/controller.h` — session lifecycle, reconnect, generations.
