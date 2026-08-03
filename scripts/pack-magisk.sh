#!/usr/bin/env bash
# Pack packaging/magisk + a prebuilt armv7 cb-daemon into a Magisk flashable zip.
# Does NOT invoke the NDK build — run ./scripts/build-android-armv7.sh first.
#
# Usage:
#   ./scripts/pack-magisk.sh
#   CB_DAEMON_BIN=/path/to/cb-daemon ./scripts/pack-magisk.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MAGISK_SRC="$ROOT/packaging/magisk"
OUT_DIR="$ROOT/dist/android-armv7"
OUT_ZIP="$OUT_DIR/cb-daemon-magisk.zip"
BIN="${CB_DAEMON_BIN:-$OUT_DIR/cb-daemon}"

REQUIRED_MEMBERS=(
  module.prop
  service.sh
  control.sh
  uninstall.sh
  customize.sh
  config.toml.example
  cb-daemon
  META-INF/com/google/android/update-binary
  META-INF/com/google/android/updater-script
)

if [[ ! -d "$MAGISK_SRC" ]]; then
  echo "error: Magisk source tree missing: $MAGISK_SRC" >&2
  exit 1
fi

if [[ ! -f "$BIN" ]]; then
  cat >&2 <<MSG
error: cb-daemon binary not found: $BIN

Build it first (LOCAL ONLY):
  ./scripts/build-android-armv7.sh

Or set CB_DAEMON_BIN to an existing stripped armv7 binary.
MSG
  exit 1
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/cb-daemon-magisk.XXXXXX")"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

# Stage module tree (exclude README from flash zip — docs stay in repo).
# Binary must be at stage root as cb-daemon (customize.sh expects \$MODPATH/cb-daemon).
cp -a "$MAGISK_SRC/." "$STAGE/"
rm -f "$STAGE/README.md"
cp -f "$BIN" "$STAGE/cb-daemon"
chmod 755 "$STAGE/cb-daemon" \
  "$STAGE/service.sh" \
  "$STAGE/control.sh" \
  "$STAGE/uninstall.sh" \
  "$STAGE/customize.sh" \
  "$STAGE/META-INF/com/google/android/update-binary"

mkdir -p "$OUT_DIR"
rm -f "$OUT_ZIP"

python3 - "$STAGE" "$OUT_ZIP" <<'PY'
import os, sys, zipfile
from pathlib import Path

stage = Path(sys.argv[1])
out = Path(sys.argv[2])
with zipfile.ZipFile(out, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for path in sorted(stage.rglob("*")):
        if not path.is_file():
            continue
        if path.name == "README.md":
            continue
        arcname = path.relative_to(stage).as_posix()
        zf.write(path, arcname)
print(f"Packed: {out}")
with zipfile.ZipFile(out) as zf:
    print("Members:")
    for name in zf.namelist():
        print(f"  {name}")
PY

missing=0
mapfile -t PRESENT < <(python3 - "$OUT_ZIP" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as zf:
    for n in zf.namelist():
        print(n)
PY
)

for member in "${REQUIRED_MEMBERS[@]}"; do
  found=0
  for present in "${PRESENT[@]}"; do
    if [[ "$present" == "$member" ]]; then
      found=1
      break
    fi
  done
  if [[ "$found" -eq 0 ]]; then
    echo "error: zip missing required member: $member" >&2
    missing=1
  fi
done

if [[ "$missing" -ne 0 ]]; then
  exit 1
fi

echo "Zip member sanity check: OK"
