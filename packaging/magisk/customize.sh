#!/system/bin/sh
# Magisk install hook: install binary + optional default config under /data/adb/cb-daemon.
# Binary is shipped at $MODPATH/cb-daemon (same path the pack script stages).

SKIPUNZIP=0

RUNTIME_DIR="/data/adb/cb-daemon"
RUNTIME_BIN="$RUNTIME_DIR/cb-daemon"
RUNTIME_CFG="$RUNTIME_DIR/config.toml"
MOD_BIN="$MODPATH/cb-daemon"
MOD_CFG="$MODPATH/config.toml.example"

ui_print "- Installing cb-daemon runtime to $RUNTIME_DIR"

mkdir -p "$RUNTIME_DIR"

if [ ! -f "$MOD_BIN" ]; then
  ui_print "! Missing module binary: $MOD_BIN"
  abort "! Repack the Magisk zip with dist/android-armv7/cb-daemon as cb-daemon"
fi

cp -f "$MOD_BIN" "$RUNTIME_BIN"
chmod 755 "$RUNTIME_BIN"

if [ ! -f "$RUNTIME_CFG" ]; then
  if [ -f "$MOD_CFG" ]; then
    ui_print "- Installing default config.toml (first install)"
    cp -f "$MOD_CFG" "$RUNTIME_CFG"
    chmod 644 "$RUNTIME_CFG"
  else
    ui_print "! No config.toml.example in module; create $RUNTIME_CFG manually"
  fi
else
  ui_print "- Keeping existing config.toml"
fi

ui_print "- Done. Service starts on late_start via service.sh"
