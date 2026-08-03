#!/system/bin/sh
# Magisk runtime control for aaservice SuDaemonLifecycle / operators.
#
# Contract (exit 0 = success):
#   start  — idempotent: running → ok; else spawn cb-daemon and write PID file
#   stop   — idempotent: not running → ok; else SIGTERM then SIGKILL, clear PID
#   status — succeed iff cb-daemon is running
#
# Installed to /data/adb/cb-daemon/control.sh by Magisk customize.sh.
# Prefer pidof / PID file — never `pgrep -f` with this script path (self-match).

RUNTIME_DIR="/data/adb/cb-daemon"
RUNTIME_BIN="$RUNTIME_DIR/cb-daemon"
RUNTIME_CFG="$RUNTIME_DIR/config.toml"
PID_FILE="$RUNTIME_DIR/cb-daemon.pid"

log() {
  echo "cb-daemon-control: $*" >&2
}

pid_is_daemon() {
  pid="$1"
  [ -n "$pid" ] && [ -d "/proc/$pid" ] || return 1
  if [ -r "/proc/$pid/cmdline" ]; then
    case "$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)" in
      *cb-daemon*) return 0 ;;
      *) return 1 ;;
    esac
  fi
  return 0
}

running_pid() {
  if [ -f "$PID_FILE" ]; then
    old_pid="$(cat "$PID_FILE" 2>/dev/null)"
    if pid_is_daemon "$old_pid"; then
      echo "$old_pid"
      return 0
    fi
    rm -f "$PID_FILE"
  fi
  if command -v pidof >/dev/null 2>&1; then
    for p in $(pidof cb-daemon 2>/dev/null); do
      if pid_is_daemon "$p"; then
        echo "$p"
        return 0
      fi
    done
  fi
  if command -v pgrep >/dev/null 2>&1; then
    for p in $(pgrep -x cb-daemon 2>/dev/null); do
      if pid_is_daemon "$p"; then
        echo "$p"
        return 0
      fi
    done
  fi
  return 1
}

cmd_start() {
  if pid="$(running_pid)"; then
    log "start: already running pid=$pid"
    echo "$pid" >"$PID_FILE"
    exit 0
  fi
  if [ ! -x "$RUNTIME_BIN" ]; then
    log "start: missing binary $RUNTIME_BIN"
    exit 1
  fi
  if [ ! -f "$RUNTIME_CFG" ]; then
    log "start: missing config $RUNTIME_CFG"
    exit 1
  fi
  log "start: spawning $RUNTIME_BIN"
  "$RUNTIME_BIN" --config "$RUNTIME_CFG" &
  new_pid=$!
  echo "$new_pid" >"$PID_FILE"
  sleep 1
  if ! pid_is_daemon "$new_pid"; then
    rm -f "$PID_FILE"
    log "start: process exited immediately"
    exit 1
  fi
  log "start: ok pid=$new_pid"
  exit 0
}

cmd_stop() {
  pid="$(running_pid)" || {
    log "stop: not running (ok)"
    rm -f "$PID_FILE"
    exit 0
  }
  log "stop: killing pid=$pid"
  kill "$pid" 2>/dev/null
  i=0
  while [ -d "/proc/$pid" ] && [ "$i" -lt 5 ]; do
    sleep 1
    i=$((i + 1))
  done
  if [ -d "/proc/$pid" ]; then
    kill -9 "$pid" 2>/dev/null
    sleep 1
  fi
  rm -f "$PID_FILE"
  if [ -d "/proc/$pid" ]; then
    log "stop: still alive after SIGKILL"
    exit 1
  fi
  log "stop: ok"
  exit 0
}

cmd_status() {
  if pid="$(running_pid)"; then
    log "status: running pid=$pid"
    exit 0
  fi
  log "status: not running"
  exit 1
}

case "${1:-}" in
  start) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  *)
    log "usage: $0 {start|stop|status}"
    exit 2
    ;;
esac
