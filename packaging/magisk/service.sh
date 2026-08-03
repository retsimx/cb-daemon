#!/system/bin/sh
# Magisk late_start service: start cb-daemon after a bounded wait for USB accessory.

RUNTIME_DIR="/data/adb/cb-daemon"
CONTROL="$RUNTIME_DIR/control.sh"
ACCESSORY="/dev/usb_accessory"
WAIT_SECS=45
SLEEP_SECS=2

log() {
  echo "cb-daemon: $*" >&2
}

if [ ! -x "$CONTROL" ]; then
  log "control.sh missing or not executable: $CONTROL"
  exit 1
fi

# Idempotent — control.sh start is a no-op when already running.
if "$CONTROL" status >/dev/null 2>&1; then
  log "already running; skip start"
  exit 0
fi

elapsed=0
while [ ! -e "$ACCESSORY" ] && [ "$elapsed" -lt "$WAIT_SECS" ]; do
  sleep "$SLEEP_SECS"
  elapsed=$((elapsed + SLEEP_SECS))
done

if [ ! -e "$ACCESSORY" ]; then
  log "accessory $ACCESSORY not present after ${WAIT_SECS}s; exit"
  exit 1
fi

log "starting via control.sh"
exec "$CONTROL" start
