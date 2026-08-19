#!/usr/bin/env bash
# Builds the embedded Tailscale node as a shared library, one directory per ABI.
#
# tsnet is Tailscale's userspace node: WireGuard plus a gVisor netstack, with no
# VpnService and no root. `tsnet/bridge.go` wraps it in a small C API whose dial call
# returns a connected descriptor, which is the only thing the C++ client needs.
#
# The result is large — roughly 25 MB stripped per ABI — because it carries the Go
# runtime and the whole Tailscale stack. Building it is optional: without it the app
# still builds and runs, and the Tailscale toggle reports that the tunnel is not
# included (see core/src/net/tailscale_dialer.cpp).
#
# Usage: tools/build-tsnet-android.sh [output-dir] [api-level]
# Requires: ANDROID_NDK_ROOT (or ANDROID_HOME with an ndk/ subdirectory), Go 1.24+.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$HOME/.cache/hypeterm/tsnet-android}"
API="${2:-29}"
ABIS=("arm64-v8a" "x86_64")

if [[ -z "${ANDROID_NDK_ROOT:-}" ]]; then
  if [[ -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
    ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)"
  elif [[ -d "$HOME/Android/Sdk/ndk" ]]; then
    ANDROID_NDK_ROOT="$HOME/Android/Sdk/ndk/$(ls "$HOME/Android/Sdk/ndk" | sort -V | tail -1)"
  else
    echo "set ANDROID_NDK_ROOT (or ANDROID_HOME) first" >&2
    exit 1
  fi
fi
export ANDROID_NDK_ROOT
echo "NDK: $ANDROID_NDK_ROOT"

# Tailscale needs a newer Go than most distributions ship.
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
echo "Go: $("$GO_BIN" version)"

HOST_TAG="linux-x86_64"
case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;
esac
TOOLCHAIN="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$HOST_TAG/bin"
if [[ ! -d "$TOOLCHAIN" ]]; then
  echo "NDK toolchain not found at $TOOLCHAIN" >&2
  exit 1
fi

for abi in "${ABIS[@]}"; do
  case "$abi" in
    arm64-v8a) goarch="arm64"; triple="aarch64-linux-android" ;;
    x86_64)    goarch="amd64"; triple="x86_64-linux-android" ;;
    *) echo "unsupported ABI $abi" >&2; exit 1 ;;
  esac

  dest="$OUT_DIR/$abi"
  mkdir -p "$dest"
  echo "Building $abi (GOARCH=$goarch, API $API)"

  # -s -w drop the symbol table and DWARF: about a quarter of the size, and the app
  # never symbolises Go frames anyway.
  # max-page-size keeps the library loadable on 16 KB-page devices (Android 15+).
  (
    cd "$REPO_ROOT/tsnet"
    CGO_ENABLED=1 \
    GOOS=android \
    GOARCH="$goarch" \
    CC="$TOOLCHAIN/${triple}${API}-clang" \
    CXX="$TOOLCHAIN/${triple}${API}-clang++" \
    "$GO_BIN" build \
      -buildmode=c-shared \
      -trimpath \
      -ldflags "-s -w -extldflags=-Wl,-z,max-page-size=16384,-z,common-page-size=16384" \
      -o "$dest/libhypeterm_tsnet.so" \
      .
  )

  # The generated header names the ABI-specific build directory in a comment only; the
  # declarations are identical, so one copy at the root is what CMake includes.
  mv -f "$dest/libhypeterm_tsnet.h" "$OUT_DIR/hypeterm_tsnet.h"

  size="$(du -h "$dest/libhypeterm_tsnet.so" | cut -f1)"
  echo "  $dest/libhypeterm_tsnet.so ($size)"
done

echo
echo "Done. Point the build at it with:"
echo "  ./gradlew assembleDebug -PtsnetRoot=$OUT_DIR"
echo "or set tsnet.dir=$OUT_DIR in local.properties."
