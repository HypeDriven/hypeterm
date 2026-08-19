# Building and running the Android application

The C++ core and its whole test suite build with nothing but CMake, a compiler and
OpenSSL (see `README.md`). This document covers the Android application.

> **State of this build.** The app builds, installs and runs. It has been exercised on
> a Galaxy S24 Ultra (SM-S928B, Android 16, arm64-v8a) against the fake relay: pairing,
> authentication, attach, rendering, keyboard input, resize, rotation, reconnect and
> surface loss. What has *not* been measured is spec §14's performance numbers, which
> need an agreed reference device and a profiler — see `docs/acceptance.md`.

## Prerequisites

| Component | Version used | Notes |
| --- | --- | --- |
| Android SDK platform | 35 | `compileSdk`/`targetSdk` |
| Build tools | 35.0.0 | |
| NDK | 27.0.12077973 | r28 also works and needs no 16 KB flag |
| CMake | 3.22.1 | the SDK-provided one |
| JDK | 21 | Gradle 8.11 and AGP 8.7 both accept it |
| Gradle | 8.11.1 | via `./gradlew` |

Install the SDK pieces with the command-line tools:

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
yes | "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --licenses
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install \
  "platform-tools" "platforms;android-35" "build-tools;35.0.0" \
  "ndk;27.0.12077973" "cmake;3.22.1"
echo "sdk.dir=$ANDROID_HOME" > local.properties
```

## OpenSSL

The NDK ships no crypto library, so OpenSSL is built once per ABI and linked
**statically**:

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
tools/build-openssl-android.sh                 # ~2 minutes for both ABIs
# builds into $HOME/.cache/hypeterm/openssl-android, where the Gradle build looks
```

Three reasons for building rather than using the published
`com.android.ndk.thirdparty:openssl` prefab package:

1. That package is OpenSSL 1.1.1, which is end of life.
2. It was built before 16 KB page alignment existed, and Android 15+ rejects
   unaligned libraries — the device shows an "ELF alignment check failed" dialog.
3. Static linking leaves the APK with exactly one native library
   (`libhypeterm.so`) instead of four.

The build fails with a pointer to the script if that directory is missing. To keep it
elsewhere, set `openssl.dir` in `local.properties` (not committed) or pass
`-Ptm.openssl.root=…`.

### 16 KB page size

`ANDROID_STL=c++_static` and `-Wl,-z,max-page-size=16384` together mean the APK carries
one 16 KB-aligned library. Verify after a build:

```bash
unzip -o app/build/outputs/apk/debug/app-debug.apk 'lib/arm64-v8a/*' -d /tmp/apk
$ANDROID_HOME/ndk/27.0.12077973/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf \
  -l /tmp/apk/lib/arm64-v8a/libhypeterm.so | grep LOAD
```

Every LOAD segment must show alignment `0x4000`.

## Tailscale (optional)

The embedded node lets the app reach a relay inside a tailnet. It is a separate Go
library and the build works without it, so this step is optional:

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
tools/build-tsnet-android.sh                   # ~2 minutes for both ABIs
```

Gradle picks it up from `~/.cache/hypeterm/tsnet-android` automatically. Override with
`-PtsnetRoot=…` or `tsnet.dir=…` in `local.properties`. The result is packaged as
`lib/<abi>/libhypeterm_tsnet.so` and loaded with `dlopen`; when it is absent the app
reports that the tunnel is not included rather than connecting directly.

It needs Go 1.24 or newer — Tailscale will not build on the Go most distributions ship,
and the toolchain downloads a newer one on demand. Set `GO=/path/to/go` if the right
one is not on `PATH`.

It is not free: about **21 MB per ABI**, which roughly quadruples the native payload.
Build it only when the tunnel is wanted.

For host tests, `tools/build-tsnet-host.sh` builds the same library for this machine;
`tests/integration/test_tailscale.cpp` picks it up from `~/.cache/hypeterm/tsnet-host`
(or `HYPETERM_TSNET_LIB`) and skips when it is missing.

Verify alignment the same way as OpenSSL:

```bash
$ANDROID_HOME/ndk/27.0.12077973/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf \
  -l ~/.cache/hypeterm/tsnet-android/arm64-v8a/libhypeterm_tsnet.so | grep LOAD
```

## Building and installing

```bash
./gradlew :app:assembleDebug
./gradlew :app:installDebug            # with a device attached
```

On WSL2 the USB device belongs to Windows, so use the Windows `adb` and hand it a
Windows path:

```bash
ADB="/mnt/c/Users/<you>/AppData/Local/Android/Sdk/platform-tools/adb.exe"
cp app/build/outputs/apk/debug/app-debug.apk /mnt/c/Users/<you>/AppData/Local/Temp/tm.apk
"$ADB" install -r 'C:\Users\<you>\AppData\Local\Temp\tm.apk'
```

## Running it against the fake relay

No production relay is needed to exercise the whole client. `adb reverse` puts the fake
relay on the *device's own loopback*, which also satisfies the client's rule that
cleartext is only ever spoken to a loopback host (spec §7.4).

```bash
python3 tools/fake_relay/relay.py --host 0.0.0.0 --port 18443 \
  --origin http://127.0.0.1:18443 &
"$ADB" reverse tcp:18443 tcp:18443
"$ADB" shell 'curl -s http://127.0.0.1:18443/healthz'      # {"status": "ok"}
```

Pair the app the same way a person does: with a code. The fake relay prints one on
stderr as it starts, and will mint another on request:

```bash
curl -sX POST http://127.0.0.1:18443/_test/pair-code -d '{}' | jq -r .code
# → HT1.eyJ1IjogImh0dHA6Ly8xMjcuMC4wLjE6MTg0NDMi...
```

Paste it into the app and tap **Pair this device**. Nothing else has to be entered: the
device generates its own key, and the code carries the relay address. There is no
"generate a key" step any more — the app never had one that worked against the real
relay, which requires a device to sign its own registration.

Then give it something to mirror:

```bash
TID=$(curl -sX POST http://127.0.0.1:18443/_test/terminals \
  -H 'Content-Type: application/json' -d '{"label":"build shell"}' | jq -r .terminal_id)
curl -sX POST http://127.0.0.1:18443/_test/emit -H 'Content-Type: application/json' \
  -d "{\"terminal_id\":\"$TID\",\"text\":\"hello from the relay\\r\\n$ \"}"
```

Useful `/_test/` endpoints while poking at it: `emit`, `resize`, `close`, `drop` (force
a disconnect), `evict` (force a `gap`), `input/<id>` (see what the phone sent),
`policy` (force `offset_ahead`, refuse input, serve version 1 only, decline resizes).

`tools/device_ui.py` drives the screens by element text rather than coordinates, which
survives layout changes:

```bash
python3 tools/device_ui.py dump                       # every visible element
python3 tools/device_ui.py tap  "Pair this device"
python3 tools/device_ui.py text "Public key:"
python3 tools/device_ui.py type "Identity ID" "2c-_fjX…"
```

## Verifying the tunnel on a device

The node can be driven without the app's UI, which is the quickest way to tell whether
a phone will host it at all:

```bash
adb push libhypeterm_tsnet.so /data/local/tmp/
adb shell /data/local/tmp/nodeprobe /data/local/tmp/libhypeterm_tsnet.so /data/local/tmp/state
```

On a Galaxy S24 Ultra (Android 16) this reports all 46 interfaces, starts the node, and
reaches `NeedsLogin` with a `https://login.tailscale.com/a/…` URL after roughly 25
seconds. In the app the same sequence appears in `adb logcat -s Hypeterm`:

```
tm.tailscale: the embedded Tailscale node is starting
tm.tailscale: node state: NeedsLogin as localhost
tm.tailscale: the node is waiting to be authorised in a browser
```

`localhost` is what Android answers for the OS host name; once the node joins a tailnet
the status reports its MagicDNS name instead.

Note that `getifaddrs` and the storage workarounds are the *only* reasons this works —
see the Tailscale entries in `CLAUDE.md` under "Things that bite".

## Things that needed fixing on the first real device, in case they recur

1. **The relay URL never reached the controller.** The native controller is built when
   the process starts, before the user has typed a URL; `NativeBridge.setServerUrl`
   exists for exactly that and must be called before `start()`.
2. **`hasCredentials()` before `start()`** returned false on a cold launch until the
   controller learned to load the stored credential lazily.
3. **Edge-to-edge.** With `targetSdk = 35` the window is edge-to-edge: without
   `applySystemWindowPadding()` the status line sits under the clock and the extra-key
   row under the navigation bar.
4. **`Bitmap.Config.ALPHA_8` row padding.** `rowBytes` can exceed the width, so the
   rasterizer repacks rows before handing the bitmap to the atlas.
5. **Non-exported activities.** `adb shell am start` cannot launch `TerminalActivity`
   directly; go through the launcher activity.

## Instrumentation tests worth adding

Still device-only, and not yet written:

- A forced EGL context loss (spec §17.7). Surface destroy/recreate is exercised; a true
  `EGL_CONTEXT_LOST` is not.
- IME behaviour across several keyboards: composition, dead keys, and the duplicate
  key/commit pair the filter in `KeyEncoder` exists for.
- TalkBack reading the grid and the extra-key row's latch state.
- The §14 performance numbers on the agreed reference device.
