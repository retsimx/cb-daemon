# cb-daemon Magisk module

Late_start Magisk service that runs `cb-daemon` on Android (armv7), intended for
devices such as the Samsung SM-T350 tablet.

## Runtime paths

| Path | Purpose |
|------|---------|
| `/data/adb/cb-daemon/cb-daemon` | Stripped daemon binary |
| `/data/adb/cb-daemon/config.toml` | Config (created on first install only) |
| `/data/adb/cb-daemon/control.sh` | start / stop / status (aaservice + ops) |
| `/data/adb/cb-daemon/cb-daemon.pid` | PID file written by `control.sh` |
| `/data/adb/cb-daemon/cb-daemon.log` | Daemon stdout/stderr (rotated at 5 MiB) |

Module scripts live under Magisk’s module directory; the binary, `control.sh`,
and config are copied to `/data/adb/cb-daemon/` by `customize.sh` at install time.
`service.sh` waits for `/dev/usb_accessory`, then calls `control.sh start`.

## Ops rules (USB accessory)

- **Only one owner** of `/dev/usb_accessory` at a time.
- **Stop via `control.sh stop`** before aaservice (or any other client) claims USB
  accessory mode. Leaving both running causes open/read failures and flaky AOA.
- Default config uses `backend = "aoa"` and `device = "/dev/usb_accessory"`.

## aaservice contract

`SuDaemonLifecycle` shells:

```text
su -c '/data/adb/cb-daemon/control.sh start'
su -c '/data/adb/cb-daemon/control.sh stop'
su -c '/data/adb/cb-daemon/control.sh status'
```

Exit `0` = success. `start` / `stop` are idempotent.

## Build & pack (developer host)

Android NDK builds are **local only** (not CI).

```bash
./scripts/build-android-armv7.sh   # → dist/android-armv7/cb-daemon
./scripts/pack-magisk.sh           # → dist/android-armv7/cb-daemon-magisk.zip
```

Flash `cb-daemon-magisk.zip` in Magisk. Requires Magisk v20.4+.

## Device checklist

Under Magisk/`su` context:

1. Confirm module enabled and reboot once after install.
2. Check binary, `control.sh`, and config exist under `/data/adb/cb-daemon/`.
3. If accessory is present, late_start `service.sh` should start the daemon after
   a bounded wait (~45s). Check: `/data/adb/cb-daemon/control.sh status`.
4. Manual:  
   `/data/adb/cb-daemon/control.sh start|stop|status`
5. Uninstall module via Magisk; confirm `/data/adb/cb-daemon/` is removed.
