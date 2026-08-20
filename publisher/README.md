# hypeterm-publish

Hosts a terminal on this machine and mirrors it through a Terminal Mirror Relay, so a
phone can watch it and type into it.

One binary covers Windows and Unix: `portable-pty` gives the same interface over ConPTY
and `forkpty`, which is what lets the same tool mirror PowerShell, `cmd`, and a shell
inside WSL.

## What it can and cannot mirror

**It hosts the session.** A terminal is mirrored because this program started it, owns
its pseudo-terminal, and copies the bytes both ways.

**It cannot attach to a terminal that is already running.** A terminal's byte stream
belongs to whatever created its pseudo-terminal — Windows Terminal, or your login
shell. Nothing else can read it after the fact without debugger-level privileges, and
Windows Terminal exposes no way to share or duplicate a running tab. So a session you
opened before starting this tool stays unmirrored.

In practice that matters for about a day: point a terminal profile at
`hypeterm-publish` once (below) and a tab opened from it is mirrored from the moment it
starts, with no per-session step. As many tabs as you like, all at once.

## Setting up

The relay decides who may publish, so this machine enrols once:

```bash
hypeterm-publish enroll --relay https://hypeterm-relay.example.ts.net
```

That creates an identity if this machine has none, generates a separate device key,
registers it as a `publisher`, and writes both to a private state file
(`~/.config/hypeterm/publisher.json`, or `%APPDATA%\hypeterm\publisher.json`). The file
holds two private keys and is refused if it is readable by anyone else.

## Pairing a phone

```bash
hypeterm-publish pair-code
```

Paste the `HT1.…` string into the phone's pairing screen. It carries the relay address,
the identity, and a short-lived token — treat it like a password until it expires,
which is a matter of minutes.

The phone still signs its own registration challenge, so its private key never leaves
it and this machine never sees it. The code only lends the phone this identity's
authority long enough to ask.

When the phone reaches the relay at a different address than this machine does — a
Tailscale sidecar is reached by its MagicDNS name from the tailnet, and by something
else from the host running it — say so:

```bash
hypeterm-publish pair-code --url https://hypeterm-relay.example.ts.net
```

## Publishing a terminal

```bash
hypeterm-publish run                       # your shell
hypeterm-publish run --label "build" -- cargo watch -x test
```

It is a transparent wrapper: you use the terminal exactly as before, and it is mirrored
while you do. The phone renders the terminal at *this* machine's size and zooms around
it rather than reshaping the session (client spec §10.4), so the window you are working
in is never resized underneath you.

`hypeterm-publish list` shows what is currently published.

### Many terminals at once

A device may hold **one** publisher connection to the relay (relay spec §6.1). That one
connection is built to carry many terminals — publisher frames name a terminal by UUID
precisely so it can — so the constraint is on connections, not on terminals.

One process therefore owns it on behalf of them all. The first `run` on a machine
starts a small daemon; every `run` after that hands it a terminal over a private local
socket, and the daemon multiplexes them onto the single connection. Nothing about this
is visible in normal use: `run` starts the daemon if it is not there, and the daemon
stands down a minute after the last terminal closes.

```bash
hypeterm-publish daemon --status      # is one running, and where is its log
hypeterm-publish daemon --stop        # ask it to stand down (refused while mirroring)
hypeterm-publish daemon --foreground  # run it here, with its log on stderr
```

Each `run` keeps its own pseudo-terminal and its own child process, so closing a tab
still ends exactly that shell and nothing else. What crosses the socket is bytes — and
each terminal's *offset*, because the bytes not yet acknowledged durable are retained
in the `run` that produced them. That placement is deliberate: if they lived in the
daemon, the daemon dying would leave the relay's offsets contiguous while the bytes
behind them were gone, which is a hole in the stream that nothing downstream could
detect. Held beside the shell, they are simply sent again.

If the daemon does go away — killed, or upgraded out from under a running tab — each
`run` notices, waits, and attaches to a new one under the same reference. The relay
recognises the terminal as the same one and says where its stream had got to, and the
retained bytes go out again from there: the phone's list does not grow a second row and
the scrollback does not restart. Killing the daemon mid-stream and checking the relay's
final byte count against the shell's exact output is how that is verified.

`run` never publishes directly when a daemon could exist. A second publisher connection
would take the device over from the daemon (that is what §6.1 means by *supersede*), and
every other mirrored tab on the machine would go dark because of one terminal. If the
daemon cannot be reached at all, `run` says so and hosts the shell unmirrored — one tab
without a mirror is a much smaller loss than four.

On **Windows** there is no daemon: `run` publishes directly, one terminal at a time, and
a second one supersedes the first. A shell inside WSL runs the Linux build and mirrors
as many as you like, which is the better arrangement there anyway (see below).

### Terminals a phone opens

A paired phone can ask this machine for a terminal, but only once somebody sitting here
has said so — the relay may ask whatever it likes, and nothing spawns without this:

```bash
hypeterm-publish remote-open --enable --shell /bin/bash --max 4
hypeterm-publish remote-open --status     # the policy, and what it resolves to here
hypeterm-publish remote-open --disable
```

The machine alone decides what runs: the request carries a label and a size and no
command, working directory or environment (relay spec §4.6). Those come from what was
recorded above.

Such a terminal also opens **on this machine's screen**, so the same shell is in front
of whoever is sitting here and on the phone — which is what a mirror means. A second one
opens as a **tab** beside the first wherever the emulator has tabs, rather than another
window; under WSL they share a Windows Terminal window named `hypeterm`, which keeps
them out of the window you are working in. `--window` chooses the emulator:

| `--window` | |
| --- | --- |
| `auto` (default) | Windows Terminal under WSL; otherwise `x-terminal-emulator`, `gnome-terminal`, `konsole`, `xfce4-terminal`, `wezterm`, `alacritty`, `kitty`, `foot` or `xterm`, whichever is on `PATH` |
| `never` | always host it headlessly |
| a command | the emulator to use, ending with its "and then run this" flag: `--window konsole -e`. Tabs are then yours to ask for — `--window wt.exe -w new wsl.exe -d Ubuntu-24.04 --` gets a window each |

A window is only tried where one could appear — under WSL, or with `DISPLAY` /
`WAYLAND_DISPLAY` set — so a server hosts headlessly without being told to. If the
emulator fails anyway (no display, a missing font) the shell is hosted headlessly
instead: the phone asked for a terminal, and one without a window here is still the
terminal it asked for. The daemon's `remote-opens.log` records what happened.

Two consequences of the window being real. Its size is the terminal's size, not the
size the phone asked for, because the publisher is the sole authority over the
dimensions (relay spec §6.5) and the phone renders whatever grid it is sent. And closing
a tab ends that shell, exactly as closing any terminal does.

Labels carrying a `;` get no window. Windows Terminal reads one as the separator
between its own commands, so an argument holding one would arrive split in two — and
that label came off the network. It is refused rather than escaped.

### Windows Terminal

Add one profile. Settings → *Open JSON file*, then add to `profiles.list`:

```json
{
  "name": "Mirrored shell",
  "commandline": "wsl.exe -d Ubuntu-24.04 --cd ~ -- /home/you/.local/bin/hypeterm-publish run -- bash -l"
}
```

Every tab opened with that profile is mirrored from the moment it starts, and they are
all mirrored at the same time. Labels get the working directory and process id appended,
so a row of them is still tellable apart in the client's list.

A shell inside WSL can equally run the Linux build and publish itself, which is the
better choice when the terminal you care about is a WSL one: the pseudo-terminal is
then the Linux one the shell actually has, rather than the ConPTY wrapping `wsl.exe`.

## Building

```bash
cargo build --release                                    # this machine
cargo build --release --target x86_64-pc-windows-gnu     # Windows, from Linux
```

The Windows cross build needs `mingw-w64` and the `x86_64-pc-windows-gnu` target.

## Testing

```bash
cargo test                                               # unit tests, no relay needed
HYPETERM_TEST_RELAY=http://127.0.0.1:9080 cargo test     # plus the end-to-end tests
```

`tests/end_to_end.rs` publishes a real pseudo-terminal, subscribes the way the Android
client does, and checks that the bytes arrive and that typed input reaches the shell. It
also publishes two terminals over one connection and checks that typing into one reaches
that shell and not the other — the failure multiplexing exists to prevent, and one that
used to be silent, because the relay addresses input to a *device* and a publisher that
did not own the named terminal simply dropped it.

The last of them starts a real `run`, lets a shell write, kills its daemon outright, and
then reads the whole terminal back from the relay asserting that every frame begins
exactly where the previous one ended. That is the property the daemon is arranged
around: the bytes not yet acknowledged durable are held in `run`, so losing the daemon
interrupts a mirror but cannot put a hole in one. Comparing only the text at the end
would not do — a stream that skipped forward and one that repeated itself can both still
contain every line, and both are corruption no subscriber could recover from. It gives
itself a runtime directory under `/tmp`, because a unix socket path must fit in 108
bytes and a deep `$TMPDIR` does not leave room.

The tests skip themselves when no relay is configured; `cd ../server && just up local`
starts one. They share a single identity per test binary and register a device each,
because a relay allows ten identity registrations an hour per source address
(`ratelimit.identity_registrations_per_hour_per_source`) and one per test meant the
suite could be run twice an hour before failing with `429` for reasons that had nothing
to do with the code. A device is what actually has to be separate — the relay allows one
publisher connection per device, so tests sharing one would supersede each other.

`tests/protocol.rs` compares the frame encoders against the relay's own implementation
rather than against a second reading of the specification, and `src/pairing.rs` asserts
the same pairing-code vector the client's test asserts, so neither pair can drift.
