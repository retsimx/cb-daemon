#!/usr/bin/env bash
# LOCAL ONLY — not used by CI. Requires Android NDK + cargo-ndk on the developer machine.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="armv7-linux-androideabi"
CRATES=(aa-crc aa-frame aa-registers aa-link)

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

for crate in "${CRATES[@]}"; do
  cargo ndk --target "$TARGET" -- build -p "$crate" --release
done
