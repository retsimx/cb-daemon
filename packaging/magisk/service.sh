#!/system/bin/sh
# Magisk late_start service: start cb-daemon after a bounded wait for USB accessory.

RUNTIME_DIR="/data/adb/cb-daemon"
RUNTIME_BIN="$RUNTIME_DIR/cb-daemon"
RUNTIME_CFG="$RUNTIME_DIR/config.toml"
PID_FILE="$RUNTIME_DIR/cb-daemon.pid"
ACCESSORY="/dev/usb_accessory"
WAIT_SECS=45
SLEEP_SECS=2

log() {
  echo "cb-daemon: $*" >&2
}

already_running() {
  if [ -f "$PID_FILE" ]; then
    old_pid="$(cat "$PID_FILE" 2>/dev/null)"
    if [ -n "$old_pid" ] && [ -d "/proc/$old_pid" ]; then
      return 0
    fi
    rm -f "$PID_FILE"
  fi
  if [ -x "$RUNTIME_BIN" ]; then
    # Match this exact binary path when busybox/toolbox pgrep is available.
    if command -v pgrep >/dev/null 2>&1; then
      pgrep -f "$RUNTIME_BIN" >/dev/null 2>&1 && return 0
    fi
  fi
  return 1
}

if [ ! -x "$RUNTIME_BIN" ]; then
  log "binary missing or not executable: $RUNTIME_BIN"
  exit 1
fi

if [ ! -f "$RUNTIME_CFG" ]; then
  log "config missing: $RUNTIME_CFG"
  exit 1
fi

if already_running; then
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

log "starting $RUNTIME_BIN"
"$RUNTIME_BIN" --config "$RUNTIME_CFG" &
echo $! >"$PID_FILE"
exit 0
