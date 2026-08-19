# Architecture

Implements spec §6. This document describes what exists, not what is planned.

## Components

```
                 ┌──────────────────────── Android host layer (Kotlin) ───────────────────┐
                 │ Activities · TerminalView (surface, IME, touch, accessibility)          │
                 │ KeystoreSecureStore · GlyphRasterizer · ConnectivityWatcher · clipboard │
                 └───────────────┬──────────────────────────────┬───────────────────────────┘
                                 │ JNI (app/src/main/cpp)       │
                 ┌───────────────▼──────────────────────────────▼───────────────────────────┐
                 │ app::Controller — session lifecycle, reconnect, generations, commands    │
                 └───┬─────────────────┬────────────────┬───────────────────┬────────────────┘
                     │                 │                │                   │
        ┌────────────▼───┐   ┌─────────▼──────┐  ┌──────▼────────┐  ┌───────▼────────┐
        │ api::Relay     │   │ api::Mirror    │  │ term::        │  │ app::Terminal  │
        │ Client (HTTP)  │   │ Session (WS)   │  │ Emulator      │  │ Session (view) │
        └────────┬───────┘   └────────┬───────┘  └──────┬────────┘  └───────┬────────┘
                 │                    │                 │                   │
        ┌────────▼────────────────────▼─────┐   ┌───────▼──────────┐  ┌─────▼──────────┐
        │ net:: TLS · HTTP/1.1 · WebSocket  │   │ term:: parser,   │  │ render:: frame │
        │ crypto:: Ed25519 · SHA-256        │   │ screen, scroll   │  │ builder, atlas │
        └────────┬──────────────────────────┘   └──────────────────┘  └────────────────┘
                 │ net::Dialer (optional)
        ┌────────▼──────────────────────────┐
        │ net::TailscaleDialer → tsnet/     │  user-space WireGuard, Go, dlopen'd
        └───────────────────────────────────┘
```

`api::RelayClient` and `api::MirrorSession` are the API adapter of spec §7.1: they are
the only files that know the relay's wire format. Everything above them speaks in
normalized events (`api/events.h`), and everything below them is transport.

`net::Dialer` is the tunnel seam (spec §7.4.1). It answers one question — "give me a
connected descriptor for this host and port" — so an embedded Tailscale node can carry
the connection without any layer above knowing. `TcpTransport::Adopt` takes the
descriptor and behaves exactly as it does for a socket it connected itself. When no
dialer is configured the pointer is null and nothing changes.

## Threads

Four, exactly as spec §6.2 requires.

| Thread | Owns | Never does |
| --- | --- | --- |
| Android UI | Activities, IME callbacks, touch | Blocking I/O, terminal parsing, GL |
| Network/parser (`Controller::NetworkThreadMain`) | Sockets, the relay protocol, the emulator, snapshot production | GL, JVM UI calls |
| Render (`android::RenderThread`) | EGL context, every GL call, the glyph atlas | Network I/O, terminal parsing |
| Rasterization | Runs on the render thread inside a bounded budget | Unbounded font work in a frame |

### What crosses between them

- **UI → network**: one bounded, ordered command queue (`BoundedQueue<Command>`). Key
  events, text, paste, resize, scroll, selection and lifecycle all travel as commands.
  Input is never sent from a UI callback (spec §6.2), and a full queue is *reported*,
  never silently dropped (spec §9.3).
- **Network → render**: an immutable `term::Snapshot` per publish. Lines inside it are
  `shared_ptr<const Line>` into the emulator's copy-on-write storage, so producing one
  costs a pointer copy per visible row and the renderer can hold it while parsing
  continues.
- **Render → network**: the computed grid size, when it changes, which the controller
  debounces and turns into a resize *request* (spec §10.3).
- **Render → UI**: whether the view is still following the newest output (spec §5.2).
  Edge-triggered — it fires only when the answer changes, so an idle terminal costs no
  JNI — and the render thread is the only one that can answer it, because following is
  the user's intent (view state it owns) *and* a session parked at the live bottom,
  which reaches it inside the snapshot.
- **Network → UI**: status, terminal list, title, user-visible messages, bell — each
  marshalled to the main thread by the Kotlin layer.

The read loop does not poll. `net::Notifier` is a self-pipe that the transport polls
alongside the socket, so queueing a keystroke wakes a blocked read immediately; the
read deadline exists only to publish a pending frame inside the coalescing window.

## Data flow of one byte of terminal output

1. `WebSocketClient::ReadMessage` returns a binary frame.
2. `MirrorSession::HandleBinaryFrame` checks the frame type, reads the u64 start
   offset, discards anything already applied, and emits `kOutput` with the new suffix.
3. `Controller::HandleMirrorEvent` hands the bytes to `TerminalSession::ApplyOutput`.
4. `term::Parser` drives `term::Emulator`, which mutates the active `term::Screen`.
   Lines that scroll off the primary screen move into `term::Scrollback`.
5. Within the coalescing window the controller publishes a `Snapshot` and the render
   thread wakes.
6. `render::BuildFrame` turns the snapshot into background runs, glyph quads,
   decorations and a cursor; `render::GlRenderer` draws them in that order.

Nothing in steps 4–6 allocates without a bound, and step 6 never blocks on font work:
missing glyphs are queued and the frame asks for a redraw (spec §10.2).

## Generations

Every connection attempt increments `Controller::generation_`, and the mirror event
handler ignores anything whose generation is not current (spec §11). Disconnects that
originate inside a callback set a pending flag instead of destroying the session
immediately — tearing a `MirrorSession` down from inside its own read loop would be a
use-after-free.

## What the JVM layer may and may not do

May: own the window and surface, run the IME, seal credentials with the Keystore,
rasterize glyphs, read connectivity, read and write the clipboard, expose accessibility
text, and route user actions.

May not: parse terminal output, speak the relay protocol, decide reconnect policy,
manage offsets, draw the grid, or decide what a keystroke means — including how a
modifier latched in the extra-key row divides a delivery of committed IME text, which
is `input::PlanTypedText`. That one arrived in Kotlin first, and the cost was a bug
reachable only with a particular soft keyboard on a particular phone, where no test on
a developer machine could see it. Those all live in `core/` so they can be tested on a
developer machine without a device, which is also why the whole test suite runs with
`ctest` and no emulator.
