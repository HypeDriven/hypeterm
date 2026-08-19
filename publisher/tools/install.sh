#!/usr/bin/env bash
# Build and install hypeterm-publish, and say what still needs doing.
#
# The copy is a rename rather than an overwrite: the running daemon and every hosted
# shell are executing the file, so a plain `cp` fails with "Text file busy". A rename
# swaps the name and leaves the old inode alive for the processes already using it.
set -euo pipefail

DEST="${DEST:-$HOME/.local/bin}"
cd "$(dirname "$0")/.."

echo "==> building"
cargo build --release --quiet

mkdir -p "$DEST"
install -m 755 target/release/hypeterm-publish "$DEST/.hypeterm-publish.new"
mv -f "$DEST/.hypeterm-publish.new" "$DEST/hypeterm-publish"
echo "==> installed $DEST/hypeterm-publish"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "    NOTE: $DEST is not on your PATH" ;;
esac

# A running daemon keeps serving from the old inode, and after an IPC version bump a new
# `run` refuses to talk to it. Say so here rather than letting that surface later as a
# confusing error in the middle of opening a terminal.
if "$DEST/hypeterm-publish" status 2>/dev/null | grep -q '^daemon *running'; then
    cat <<'NOTE'

    A daemon from the previous build is still running. It keeps mirroring whatever it
    already has, but new terminals — and anything opened from a phone — need it
    restarted onto this build:

        hypeterm-publish daemon --stop     # refuses while terminals are mirrored
        hypeterm-publish daemon --detach

    Close your mirrored terminals first if it refuses. Losing the daemon never takes a
    shell with it, but it does end that shell's mirror.
NOTE
fi
