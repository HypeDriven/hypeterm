# Hypeterm

Watch and type into a terminal running on your machine, from your phone.

## Quick start

You need [`just`](https://github.com/casey/just), Docker with the Compose plugin, and a
Rust toolchain. Then, from this directory:

```bash
just up
```

That is the whole of it. It checks those prerequisites, asks the one question it cannot
answer for you — how the relay should be reachable — deploys the relay's container,
enrols this machine as a publisher, and prints a pairing code to type into the phone.
Then:

```bash
just run          # host a shell; it is mirrored from the moment it starts
```

Answers are remembered in `server/deploy/.env`, which is generated, kept at mode `0600`
and never committed. A later `just up` redeploys the same shape without asking again.
Nothing is prompted for that has a sensible default: bootstrap secrets are generated,
not requested, and with no terminal attached — CI, a pipe — every question takes its
default instead of hanging.

### What it asks

| Mode | What it is for | What it needs from you |
| --- | --- | --- |
| `local` | Development and single-machine use. Loopback only, no TLS. | Nothing. |
| `tailscale` | Your own devices, anywhere, with a real certificate and nothing exposed to the internet. | A Tailscale auth key, or a login link to click. |
| `proxy` | Behind a TLS-terminating reverse proxy you already run. | The public URL that proxy serves. |
| `tls` | TLS terminated in-process. | A certificate, or it offers to generate a self-signed one. |

Pick `local` to try it out and `tailscale` to actually use it — a phone cannot reach a
loopback-only relay over the network, so `local` wants `adb reverse` and a USB cable.

### Everyday recipes

```bash
just run --label build -- cargo watch -x test   # mirror one command rather than a shell
just pair                    # another pairing code, for another phone
just publishing              # what this machine has enrolled, and what it is mirroring
just status / logs / token   # the relay: health, output, operator credential
just down                    # stop the relay, keeping its data
just test                    # every test suite: relay, publisher, client core
```

`just` on its own lists them all. `server/justfile` is the full operator interface for a
deployed relay — settings, admin API, TLS, metrics — and the recipes here delegate to it.

### The Android app

```bash
just apk        # builds static OpenSSL on the first run, then :app:assembleDebug
```

This needs the Android SDK and NDK; `android/docs/android-build.md` covers the versions,
the 16 KB page-size requirement, and installing from WSL2. The app can also be run
against `android/tools/fake_relay/relay.py` over `adb reverse`, with no relay deployed at
all.

## How it works

Three pieces make that work: the machine with the terminal runs **publisher**, the phone
runs the **android** client, and a **server** in the middle relays bytes between them —
so neither side needs to reach the other directly, and the phone can pick up recent
scrollback after a reconnect.

```
publisher  ──▶  server  ──▶  android
 (hosts a PTY)  (relay + replay)  (renders, sends keystrokes)
      ◀───────────────────────────────
              typed input
```

## The projects

| Folder | What it is |
| --- | --- |
| [`android/`](android/README.md) | The Android app (`com.hypedriven.hypeterm`). A C++17 core does terminal emulation, protocol, networking and rendering; the Kotlin layer only covers platform APIs with no native equivalent (IME, Keystore, clipboard, fonts). It mirrors a remote session — it never runs a shell on the device. |
| [`publisher/`](publisher/README.md) | `hypeterm-publish`, a Rust CLI that *hosts* a terminal and mirrors it. It wraps your shell transparently over `portable-pty`, so the same binary covers ConPTY and `forkpty`. On Unix a small daemon multiplexes many tabs onto the one publisher connection the relay allows per device. It cannot attach to a terminal that was already running. |
| [`server/`](server/README.md) | Terminal Mirror Relay, a containerised Rust/axum service. It authenticates devices, fans terminal output out to subscribers over WebSockets, carries authorized input back, and keeps a bounded replay window per terminal so a reconnecting client can reconstruct recent output. It conveys bytes and does not interpret them. |

Each folder has its own README with build, test and deployment instructions, and its own
specification: `android/spec.md` for the client, `server/spec.md` for the relay, and
`server/INTEGRATION.md`, which defines how the two fit together.

## Licence

[PolyForm Noncommercial 1.0.0](LICENSE.md).
