#!/usr/bin/env bash
# Builds the embedded Tailscale node for this machine, so the host tests can exercise
# the real C API — descriptor handoff, status decoding, the refusal paths — without a
# device and without joining a tailnet.
#
# Usage: tools/build-tsnet-host.sh [output-dir]
# Requires: Go 1.24+ (Tailscale needs a newer Go than most distributions ship).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$HOME/.cache/hypeterm/tsnet-host}"

GO_BIN="${GO:-}"
if [[ -z "$GO_BIN" ]]; then
  for candidate in "$HOME/tools/go1.24/bin/go" "$(command -v go || true)"; do
    if [[ -x "$candidate" ]]; then GO_BIN="$candidate"; break; fi
  done
fi
if [[ -z "$GO_BIN" ]]; then
  echo "no Go toolchain found; set GO=/path/to/go" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
(
  cd "$REPO_ROOT/tsnet"
  CGO_ENABLED=1 "$GO_BIN" build \
    -buildmode=c-shared \
    -trimpath \
    -ldflags "-s -w" \
    -o "$OUT_DIR/libhypeterm_tsnet.so" \
    .
)
echo "$OUT_DIR/libhypeterm_tsnet.so ($(du -h "$OUT_DIR/libhypeterm_tsnet.so" | cut -f1))"
echo "tests pick this up automatically; override with HYPETERM_TSNET_LIB."
