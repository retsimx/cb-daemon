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
  "$CONTROL" start
  echo "cb-daemon-service: control.sh start exit=$?"
} >>"$LOG_FILE" 2>&1 &

exit 0
