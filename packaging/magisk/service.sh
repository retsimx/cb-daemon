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
  # The accessory may not be fully ready at boot_completed+5s on slow tablets;
  # the daemon exits if it cannot open the accessory. Retry with a bounded
  # window so a fresh boot still brings the bus up without blocking Magisk
  # (we are already detached; control.sh start is idempotent).
  ATTEMPTS=6
  RETRY_SLEEP=10
  n=0
  while [ "$n" -lt "$ATTEMPTS" ]; do
    "$CONTROL" start
    rc=$?
    echo "cb-daemon-service: control.sh start exit=$rc (attempt $((n + 1))/$ATTEMPTS)"
    # Give the daemon time to fail the accessory open before re-checking.
    sleep "$RETRY_SLEEP"
    if pidof cb-daemon >/dev/null 2>&1; then
      echo "cb-daemon-service: daemon alive after boot start"
      break
    fi
    n=$((n + 1))
  done
  if ! pidof cb-daemon >/dev/null 2>&1; then
    echo "cb-daemon-service: daemon not running after $ATTEMPTS attempts (manual control.sh start required)"
  fi
} >>"$LOG_FILE" 2>&1 &

exit 0
