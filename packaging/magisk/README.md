# cb-daemon Magisk module

Late_start Magisk service that runs `cb-daemon` on Android (armv7), intended for
devices such as the Samsung SM-T350 tablet.

## Runtime paths

| Path | Purpose |
|------|---------|
| `/data/adb/cb-daemon/cb-daemon` | Stripped daemon binary |
| `/data/adb/cb-daemon/config.toml` | Config (created on first install only) |
| `/data/adb/cb-daemon/cb-daemon.pid` | PID file written by `service.sh` |

Module scripts live under Magisk’s module directory; the binary and config are
copied to `/data/adb/cb-daemon/` by `customize.sh` at install time.

## Ops rules (USB accessory)

- **Only one owner** of `/dev/usb_accessory` at a time.
- **Stop this Magisk service** before aaservice (or any other client) claims USB
  accessory mode. Leaving both running causes open/read failures and flaky AOA.
- Default config uses `backend = "aoa"` and `device = "/dev/usb_accessory"`.

## Build & pack (developer host)

Android NDK builds are **local only** (not CI).

```bash
./scripts/build-android-armv7.sh   # → dist/android-armv7/cb-daemon
./scripts/pack-magisk.sh           # → dist/android-armv7/cb-daemon-magisk.zip
```

Flash `cb-daemon-magisk.zip` in Magisk. Requires Magisk v20.4+.

## Device checklist (manual start/stop)

Under Magisk/`su` context (no live soak required for D10 acceptance):

1. Confirm module enabled and reboot once after install.
2. Check binary and config exist under `/data/adb/cb-daemon/`.
3. If accessory is present, `service.sh` should start the daemon after a bounded
   wait (~45s). Check process / PID file.
4. Manual start:  
   `/data/adb/cb-daemon/cb-daemon --config /data/adb/cb-daemon/config.toml &`
5. Manual stop: kill via PID file or `pkill -f /data/adb/cb-daemon/cb-daemon`.
6. Uninstall module via Magisk; confirm `/data/adb/cb-daemon/` is removed.
