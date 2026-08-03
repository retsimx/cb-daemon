#!/system/bin/sh
# Magisk late_start service: start cb-daemon after USB accessory appears.
#
# Important: return immediately. A long foreground wait here can trip Magisk
# bootloop protection and leave the module disabled + Magisk in safe mode
# (su only under /debug_ramdisk, not on PATH).

RUNTIME_DIR="/data/adb/cb-daemon"
CONTROL="$RUNTIME_DIR/control.sh"
ACCESSORY="/dev/usb_accessory"
WAIT_SECS=45
SLEEP_SECS=2
LOG_FILE="$RUNTIME_DIR/service.log"

log() {
  echo "cb-daemon: $*" >&2
  echo "cb-daemon: $*" >>"$LOG_FILE" 2>/dev/null
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

# Background wait + start so Magisk late_start is not blocked for WAIT_SECS.
(
  elapsed=0
  while [ ! -e "$ACCESSORY" ] && [ "$elapsed" -lt "$WAIT_SECS" ]; do
    sleep "$SLEEP_SECS"
    elapsed=$((elapsed + SLEEP_SECS))
  done

  if [ ! -e "$ACCESSORY" ]; then
    log "accessory $ACCESSORY not present after ${WAIT_SECS}s; give up"
    exit 0
  fi

  log "starting via control.sh"
  "$CONTROL" start >>"$LOG_FILE" 2>&1
) &

log "late_start: accessory wait spawned (pid=$!)"
exit 0
