# Resize policy

Spec §8.2 requires resizing to "preserve content according to a documented policy".
This is that document. The behaviour lives in `Emulator::Resize` and
`Emulator::ReflowResize`.

## Who decides the size

The publishing device owns the PTY, so the *authoritative* size arrives from the relay
in `subscribed` and `terminal.resize`. The grid always follows what the relay reports;
if it reports nothing, the locally computed grid is used.

**The client does not ask for a different one.** `AppConfig::follow_remote_size` is on
by default, and while it is, no `terminal.resize_request` is ever sent. The reason is
not protocol politeness: a mirrored terminal usually has somebody working in front of
it, and a phone asking a 200×50 desktop session to become 55×24 reflows *their* screen
to suit a device they are not looking at. Instead the client renders the whole grid and
lets the user zoom and pan around it — see `core/include/tm/render/view.h` and spec
§10.4.

Turning it off restores the older behaviour, which suits a publisher with no screen of
its own — a headless `hypeterm-publish run` in a service, say. Requests are then
debounced (`AppConfig::resize_debounce_ms`, default 250 ms) so a rotation or an
appearing keyboard produces one request rather than thirty, and the final size is
always sent.

The publisher side takes the same view: `hypeterm-publish` follows the local terminal's
window when it has one and declines subscriber requests, and honours them only when
running headless, where the subscriber's request is the only size on offer.

Reflow below still applies — it is what happens when the *publisher* resizes.

## Primary screen: reflow

Ordinary shell output is rewrapped, because a user who rotates the phone expects their
scrollback to still read as sentences rather than as ragged fragments.

1. The scrollback and the primary screen are flattened into **logical lines**, joining
   every line whose `wrapped` flag is set to the line that follows it.
2. Trailing blank rows below the cursor are dropped, so repeated resizes do not
   accumulate empty lines.
3. Each logical line is re-split at the new width, setting `wrapped` on every chunk but
   the last.
4. The last `rows` wrapped lines become the screen; everything before them goes back
   into the scrollback, which then re-applies its own line and byte bounds.
5. The cursor is tracked by its offset within its logical line and lands on the same
   character.

Consequences worth knowing:

- **Trailing blanks are not preserved.** A line of spaces with no styling is
  indistinguishable from an empty one after a reflow.
- **A cursor past the right margin is clamped to the last column.** With deferred wrap
  the cursor can sit one position beyond the last character; after rewrapping there may
  be no such position, so it clamps rather than inventing a row.
- **Combining marks travel with their base character**, since they are re-attached by
  position within the logical line.
- **Reflow is bounded.** A single logical line cannot expand past the scrollback line
  limit, so a pathological stream cannot turn a resize into unbounded work.
- **Scrollback bounds are re-applied afterwards**, so a narrower terminal — which
  produces more lines for the same text — can evict old lines.

## Alternate screen: no reflow

The alternate screen preserves grid semantics instead (spec §8.2). Lines are truncated
or padded to the new width and rows are added or removed at the bottom. Nothing is
rewrapped and nothing moves to the scrollback, because the alternate screen has none.

This is the right behaviour for the applications that use it: `vim`, `less`, `top` and
`tmux` all redraw completely when the size changes, so rewrapping their output would
only produce a flash of nonsense before the redraw.

## Row-count-only changes

When only the row count changes, no rewrapping is needed. Blank lines below the cursor
are dropped first; if that is not enough, lines scroll off the top into the scrollback,
which is what a shell user expects when the soft keyboard appears.

## Grid computation

`render::ComputeGrid` floors the usable rectangle by the cell size. The result is
clamped to at least 1×1 — a transiently zero-sized surface is normal during rotation —
and to at most 1000×1000, so a broken metric cannot produce an absurd request.
