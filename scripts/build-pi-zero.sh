#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="arm-unknown-linux-musleabihf"
BIN="cb-daemon"

if ! command -v cross >/dev/null 2>&1; then
  cat >&2 <<'MSG'
error: `cross` is not installed or not on PATH.

Pi Zero W release builds for arm-unknown-linux-musleabihf require cross.

Install (example):
  cargo install cross --git https://github.com/cross-rs/cross

Then re-run:
  ./scripts/build-pi-zero.sh
MSG
  exit 1
fi

cross build -p "$BIN" --target "$TARGET" --release

echo "built target/$TARGET/release/$BIN"
