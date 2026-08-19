# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**Hypeterm** mirrors a terminal running on a machine to a phone, both ways. Three
programs, three languages, one wire protocol:

```
publisher (Rust)  ──▶  server (Rust/axum, containerised)  ──▶  android (C++17 core + thin Kotlin)
 hosts a PTY           relays bytes, keeps a replay window     renders, sends keystrokes
      ◀────────────────────────── typed input ──────────────────────────
```

`server/spec.md` (the relay) and `android/spec.md` (the client) are **normative** — RFC
2119 documents, not design notes. `server/RECONCILIATION.md` is the integration
contract between them; read it before changing anything protocol-shaped, in any of the
three.

**Each project has its own guidance, and it is more specific than this file:**
`server/CLAUDE.md` and `android/CLAUDE.md`. Read the one for the project you are in.
`publisher/` has none — its conventions are in this file and in `publisher/README.md`.

Only `server/` is a git repository. The root and the other two projects are not tracked,
so `git` commands run from the top level will fail or reach into `server/`.

## Commands

The root `justfile` is the onboarding path across all three; `server/justfile` is the
full operator interface for a deployed relay and the root recipes delegate to it.

```bash
just up [local|tailscale|proxy|tls]   # deploy the relay, enrol this machine, print a pairing code
just run [--label X -- cmd...]        # host a shell (or one command) and mirror it
just pair                             # another pairing code, for another phone
just publishing                       # what this machine enrolled, and what it is mirroring
just status | logs | token | down     # the relay
just test                             # all three suites
just apk                              # Android debug APK (builds static OpenSSL on first run)
```

`just up` remembers its answers in `server/deploy/.env` (mode `0600`, generated, never
committed — it also holds the relay's bootstrap secrets).

### Per project

```bash
# server/ — see server/CLAUDE.md for the per-acceptance-suite breakdown
cargo test --lib                        # fast, no sockets
cargo test --test acceptance_relay criterion_9 -- --nocapture
just check                              # fmt + clippy -D warnings + test

# publisher/
cargo test                              # unit tests, no relay needed
HYPETERM_TEST_RELAY=http://127.0.0.1:9080 cargo test    # plus tests/end_to_end.rs
cargo test --test end_to_end survives_its_daemon -- --nocapture   # one test
tools/verify-remote-open.sh             # real relay + real daemon + curl, spec §4.6
tools/install.sh                        # build and install to ~/.local/bin (renames, see below)
cargo build --release --target x86_64-pc-windows-gnu    # needs mingw-w64

# android/ — the C++ core and its whole test suite build without the Android SDK
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug && cmake --build build -j
ctest --test-dir build --output-on-failure
./build/tests/tm_unit_tests --filter=Screen
```

The publisher's end-to-end tests skip themselves when no relay is configured; start one
with `cd server && just up local`. The Android integration tests likewise skip, loudly,
without `python3` (they spawn `android/tools/fake_relay/relay.py`).

## What spans the three projects

These are the invariants that no single project's tests can protect, and they are where
cross-project changes go wrong.

**The offset model is the contract.** The relay accepts a publisher output frame *only*
when its expected start offset equals the terminal's current `next_offset`; a mismatch
appends nothing and answers with authoritative offsets. Everything else follows: the
publisher retains unacknowledged bytes and resends from wherever the relay says the
stream continues, and the client parses offsets exactly from the literal so a 64-bit
value never passes through a double. An offset is a byte count, never a message counter.

**Retained bytes live beside the PTY, never beside the connection.** In `publisher/`,
`stream.rs`/`publish.rs` hold each terminal's offset and unacknowledged bytes inside the
`run` process that owns the shell — deliberately *not* in the multiplexing daemon
(`daemon.rs`, Unix only), which owns the single relay connection a device is allowed
(relay spec §6.1). A daemon that died holding those bytes would leave the relay's offsets
contiguous with the bytes behind them gone: a hole nothing downstream could detect.
Losing the daemon must interrupt a mirror, never puncture one.

**One publisher connection per device, and a second supersedes the first.** That is why
the daemon exists on Unix and why the superseded side must *stop* rather than reconnect.
Losing the mirror never takes the shell with it.

**The four binary frame layouts differ, and the difference is the terminal UUID.** The
direction that multiplexes many terminals over one connection carries it — publisher
frames do, mirror frames do not. Output is `0x01`, input `0x02`. Crossing them misparses
everything; `server/CLAUDE.md` has the full table.

**Bytes are opaque in both directions.** No UTF-8 validation, no ANSI parsing, no newline
normalisation anywhere in the relay. Terminal *input* is never persisted, never logged,
never enters the replay buffer and never advances an offset — it carries passwords.

**Shared constructions are checked against the authority, not against a second reading of
the spec.** `publisher/tests/protocol.rs` links the relay crate as a dev-dependency and
compares encoders and signatures against its implementation (which is why the publisher's
crate versions are pinned to the relay's). The pairing-code format has a fixed vector
asserted in both `publisher/src/pairing.rs` and the Android test suite. Keep those
cross-checks working — they are what stops three implementations of one wire format from
drifting.

**Pairing needs both parties.** The owner's token authorises and the device's key signs;
a pairing code only lends the phone the identity's authority for a few minutes. No
private key ever reaches the relay or the publishing machine.

**Phone-initiated terminals are off until the machine opts in.** `hypeterm-publish
remote-open --enable` is the switch that matters: the relay may say whatever it likes,
nothing spawns locally without it. A phone that can both open terminals and type into
them can run anything the user can.

**The client never resizes the remote terminal.** The phone renders at the publisher's
grid size and zooms and pans over it (client spec §10.4). Reshaping a session someone is
working in is not the phone's decision.

## Conventions all three hold to

- **Comments explain why, not what**, and cite the spec section when the reason lives
  there (`spec §6.1`, `relay spec §4.6`). Module-level docs carry the design rationale —
  read them before changing a module; several record failures that were expensive to find.
- **Tests assert specified behaviour** and are named for what would break.
- **Nothing sensitive reaches a log** — no payloads, tokens, tickets, challenges,
  signatures or raw keys. Byte counts and correlation IDs only.
- New behaviour knobs go where each project keeps them: the relay's settings registry
  (`server/src/settings/defs.rs`, never an env var), not free-floating constants.

## Things that bite across projects

- `tools/install.sh` renames rather than copies: the running daemon and every hosted
  shell are executing the binary, so `cp` fails with "Text file busy". After an IPC
  version bump, a running daemon from the old build refuses new `run`s until restarted.
- A Unix socket path must fit in 108 bytes, so the daemon and the tests that exercise it
  derive short runtime directories rather than using a deep `$TMPDIR`.
- `accepts_input` is the publisher's opt-in to being written to at all; `input_available`
  is whether *this* subscription may type right now. Only the second decides.
- The relay rate-limits identity registrations (10/hour/source), so test suites share one
  identity and register a device each — a device is what actually has to be separate.
- `local` mode is loopback only: a phone cannot reach it over the network. Use
  `adb reverse` over USB, or `just up tailscale`.
