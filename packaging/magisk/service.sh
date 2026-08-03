#!/system/bin/sh
# Magisk late_start: must return immediately (never block).
# Start cb-daemon only after sys.boot_completed, in a detached background job.
#
# Do NOT call control.sh / touch USB in the foreground path — that previously
# tripped Magisk safe-mode (module disabled, PATH su gone).

RUNTIME_DIR="/data/adb/cb-daemon"
CONTROL="$RUNTIME_DIR/control.sh"
LOG_FILE="$RUNTIME_DIR/service.log"

{
  echo "cb-daemon-service: late_start spawn $(date 2>/dev/null || echo unknown)"
  # Wait until Android reports boot finished (non-blocking for Magisk — we already &)
  while [ "$(getprop sys.boot_completed)" != "1" ]; do
    sleep 2
  done
  # Brief settle for /dev/usb_accessory + magiskd
  sleep 5
  if [ ! -x "$CONTROL" ]; then
    echo "cb-daemon-service: missing $CONTROL"
    exit 0
  fi
  # The accessory is not always usable at boot_completed+5s on slow tablets:
  # the first read can fail with EIO, the engine exits, and the daemon process
  # stays alive as a zombie (axum keeps serving; no negotiation ever happens).
  # Retry until the engine actually negotiates, not merely until the process
  # exists. control.sh start/stop are idempotent; we are already detached so
  # Magisk never blocks.
  DAEMON_LOG="/data/adb/cb-daemon/ramlog/cb-daemon.log"
  ATTEMPTS=8
  RETRY_SLEEP=12
  n=0
  while [ "$n" -lt "$ATTEMPTS" ]; do
    "$CONTROL" start
    rc=$?
    echo "cb-daemon-service: control.sh start exit=$rc (attempt $((n + 1))/$ATTEMPTS)"
    sleep "$RETRY_SLEEP"
    # Success when the daemon process is alive AND the engine negotiated.
    # Boot log is fresh (tmpfs), so any 'negotiated' is this boot's — including
    # a daemon aaservice spawned itself via control.sh (never kill a working
    # engine just because OUR windowed marker missed it).
    if pidof cb-daemon >/dev/null 2>&1 &&
      grep -q "negotiated" "$DAEMON_LOG" 2>/dev/null; then
      echo "cb-daemon-service: daemon negotiated after boot start"
      break
    fi
    # Zombie (process alive, engine dead) or never negotiated: stop and retry.
    "$CONTROL" stop >/dev/null 2>&1 || true
    n=$((n + 1))
  done
  if ! pidof cb-daemon >/dev/null 2>&1 ||
    ! grep -q "negotiated" "$DAEMON_LOG" 2>/dev/null; then
    echo "cb-daemon-service: daemon not negotiated after $ATTEMPTS attempts (manual control.sh start required)"
  fi
} >>"$LOG_FILE" 2>&1 &

exit 0
