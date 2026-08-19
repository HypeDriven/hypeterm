#!/usr/bin/env bash
# Builds a static OpenSSL for Android, one directory per ABI.
#
# The NDK ships no crypto library, and the prebuilt `com.android.ndk.thirdparty`
# OpenSSL is 1.1.1 (end of life) packaged before 16 KB page alignment existed — which
# Android 15+ rejects. Building it here gives a current OpenSSL, 16 KB-aligned objects,
# and static libraries, so the APK carries one native library instead of three.
#
# Usage: tools/build-openssl-android.sh [output-dir] [api-level]
# Requires: ANDROID_NDK_ROOT (or ANDROID_HOME with an ndk/ subdirectory), perl, make.

set -euo pipefail

VERSION="3.0.16"
OUT_DIR="${1:-$HOME/.cache/hypeterm/openssl-android}"
API="${2:-29}"
ABIS=("arm64-v8a" "x86_64")
JOBS="$(nproc 2>/dev/null || echo 4)"

if [[ -z "${ANDROID_NDK_ROOT:-}" ]]; then
  if [[ -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
    ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)"
  else
    echo "set ANDROID_NDK_ROOT (or ANDROID_HOME) first" >&2
    exit 1
  fi
fi
export ANDROID_NDK_ROOT
echo "NDK: $ANDROID_NDK_ROOT"

HOST_TAG="linux-x86_64"
case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;
esac
export PATH="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$HOST_TAG/bin:$PATH"

WORK="${TMPDIR:-/tmp}/openssl-android-build"
mkdir -p "$WORK"
SOURCE="$WORK/openssl-$VERSION"

if [[ ! -d "$SOURCE" ]]; then
  echo "Fetching OpenSSL $VERSION"
  curl -sSL -o "$WORK/openssl.tar.gz" \
    "https://github.com/openssl/openssl/releases/download/openssl-$VERSION/openssl-$VERSION.tar.gz"
  tar xzf "$WORK/openssl.tar.gz" -C "$WORK"
fi

for abi in "${ABIS[@]}"; do
  case "$abi" in
    arm64-v8a) target="android-arm64" ;;
    x86_64)    target="android-x86_64" ;;
    armeabi-v7a) target="android-arm" ;;
    x86)       target="android-x86" ;;
    *) echo "unsupported ABI: $abi" >&2; exit 1 ;;
  esac

  prefix="$OUT_DIR/$abi"
  if [[ -f "$prefix/lib/libcrypto.a" && -f "$prefix/lib/libssl.a" ]]; then
    echo "$abi: already built"
    continue
  fi

  echo "Building $abi ($target) for API $API"
  build="$WORK/build-$abi"
  rm -rf "$build"
  mkdir -p "$build"
  (
    cd "$build"
    # 16 KB max page size is the Android 15+ requirement; static libraries keep the
    # APK to a single native library.
    "$SOURCE/Configure" "$target" "-D__ANDROID_API__=$API" \
      no-shared no-tests no-ui-console no-legacy \
      -Wl,-z,max-page-size=16384 \
      --prefix="$prefix" --openssldir="$prefix/ssl" >/dev/null
    make -j"$JOBS" >/dev/null
    make install_sw >/dev/null
  )
  echo "$abi: $(du -sh "$prefix/lib" | cut -f1) installed at $prefix"
done

echo
if [ "$OUT_DIR" = "$HOME/.cache/hypeterm/openssl-android" ]; then
  echo "Done. The Gradle build looks here by default — nothing further to set."
else
  echo "Done. Point the Gradle build at it, in local.properties (not committed):"
  echo "  echo 'openssl.dir=$OUT_DIR' >> local.properties"
fi
