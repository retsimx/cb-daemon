#!/usr/bin/env bash
# LOCAL ONLY — not used by CI.
#
# Cross-build cb-daemon for Android ARMv7 (armv7-linux-androideabi).
#
# Tested baseline (informative only — no hard version gate):
#   - Android NDK r26+ or r27 (ANDROID_NDK_HOME or ANDROID_NDK_ROOT)
#   - API level 21 (--platform 21)
#   - cargo-ndk on PATH
#
# Output: dist/android-armv7/cb-daemon (stripped via NDK llvm-strip)
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="armv7-linux-androideabi"
PLATFORM=21
CRATE="cb-daemon"
OUT_DIR="$ROOT/dist/android-armv7"
OUT_BIN="$OUT_DIR/cb-daemon"
RELEASE_BIN="$ROOT/target/$TARGET/release/$CRATE"

resolve_ndk_home() {
  if [[ -n "${ANDROID_NDK_HOME:-}" ]]; then
    printf '%s\n' "$ANDROID_NDK_HOME"
    return 0
  fi
  if [[ -n "${ANDROID_NDK_ROOT:-}" ]]; then
    printf '%s\n' "$ANDROID_NDK_ROOT"
    return 0
  fi
  return 1
}

resolve_ndk_host_tag() {
  local os arch
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *)
      echo "error: unsupported host OS for NDK prebuilt lookup: $(uname -s)" >&2
      return 1
      ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=arm64 ;;
    *)
      echo "error: unsupported host CPU for NDK prebuilt lookup: $(uname -m)" >&2
      return 1
      ;;
  esac
  printf '%s-%s\n' "$os" "$arch"
}

resolve_llvm_strip() {
  local ndk="$1"
  local host_tag candidate

  host_tag="$(resolve_ndk_host_tag)"
  candidate="$ndk/toolchains/llvm/prebuilt/$host_tag/bin/llvm-strip"
  if [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  candidate="$(find "$ndk/toolchains/llvm/prebuilt" -name llvm-strip -type f -perm -111 2>/dev/null | head -n 1 || true)"
  if [[ -n "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  echo "error: NDK llvm-strip not found under: $ndk/toolchains/llvm/prebuilt" >&2
  echo "       Expected e.g. $ndk/toolchains/llvm/prebuilt/$host_tag/bin/llvm-strip" >&2
  return 1
}

if ! command -v cargo-ndk >/dev/null 2>&1; then
  cat >&2 <<'MSG'
error: `cargo-ndk` is not installed or not on PATH.

Android ARMv7 local builds require cargo-ndk and an Android NDK install.
This script is LOCAL ONLY (not wired into CI).

Install (example):
  cargo install cargo-ndk

Also ensure ANDROID_NDK_HOME (or ANDROID_NDK_ROOT) points at a valid NDK.

Then re-run:
  ./scripts/build-android-armv7.sh
MSG
  exit 1
fi

if ! NDK_HOME="$(resolve_ndk_home)"; then
  cat >&2 <<'MSG'
error: Android NDK location is not configured.

Set ANDROID_NDK_HOME or ANDROID_NDK_ROOT to your NDK install root.
This script is LOCAL ONLY (not wired into CI).

Example:
  export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/27.0.12077973"

Then re-run:
  ./scripts/build-android-armv7.sh
MSG
  exit 1
fi

if [[ ! -d "$NDK_HOME" ]]; then
  echo "error: NDK directory does not exist: $NDK_HOME" >&2
  exit 1
fi

echo "Building $CRATE for $TARGET (API $PLATFORM) — LOCAL ONLY, not CI"
cargo ndk --target "$TARGET" --platform "$PLATFORM" -- build -p "$CRATE" --release

if [[ ! -f "$RELEASE_BIN" ]]; then
  echo "error: release binary not found after build: $RELEASE_BIN" >&2
  exit 1
fi

LLVM_STRIP="$(resolve_llvm_strip "$NDK_HOME")"
echo "Stripping with: $LLVM_STRIP"
"$LLVM_STRIP" "$RELEASE_BIN"

mkdir -p "$OUT_DIR"
cp -f "$RELEASE_BIN" "$OUT_BIN"
chmod +x "$OUT_BIN"

echo "Wrote stripped binary: $OUT_BIN"
