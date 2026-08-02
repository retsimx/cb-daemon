#!/system/bin/sh
# Magisk uninstall: stop cb-daemon and remove /data/adb/cb-daemon runtime files.

RUNTIME_DIR="/data/adb/cb-daemon"
RUNTIME_BIN="$RUNTIME_DIR/cb-daemon"
PID_FILE="$RUNTIME_DIR/cb-daemon.pid"

if [ -f "$PID_FILE" ]; then
  old_pid="$(cat "$PID_FILE" 2>/dev/null)"
  if [ -n "$old_pid" ] && [ -d "/proc/$old_pid" ]; then
    kill "$old_pid" 2>/dev/null
    sleep 1
    if [ -d "/proc/$old_pid" ]; then
      kill -9 "$old_pid" 2>/dev/null
    fi
  fi
  rm -f "$PID_FILE"
fi

if command -v pkill >/dev/null 2>&1 && [ -x "$RUNTIME_BIN" ]; then
  pkill -f "$RUNTIME_BIN" 2>/dev/null
fi

rm -rf "$RUNTIME_DIR"
