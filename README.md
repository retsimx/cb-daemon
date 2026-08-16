# cb-daemon

Rust Control Box (CB) mailbox sync daemon for Advantage Air air-conditioning systems. It acts as the RS-485 "tablet" endpoint on the CB bus: it runs the CB↔tablet session (negotiate / dump / steady poll), maintains an in-memory register bank, and serves a typed JSON mailbox API over WebSocket.

In production since **2026-08-12** as the sole talker on the CB bus — a Raspberry Pi Zero W (Alpine/OpenRC) with a USB-RS485 dongle (see [Deployment](#deployment)).

## Workspace

Current Cargo workspace members:

- **`aa-crc`** — CRC-8 used on CB frames
- **`aa-frame`** — `<U>…</U=xx>` frame encode/decode and burst scanning
- **`aa-registers`** — register IDs, CAN2 wire codec, register bank, and typed decode/encode for all known registers (write-policy metadata included)
- **`aa-link`** — async byte I/O seam (`Link`), `MockLink` for hardware-free tests, `AoaLink` for raw `/dev/usb_accessory` (config on open, chunked writes; aaservice must not hold the accessory while open), and `TtyLink` for Linux USB-serial / USB-RS485 (57600 8N1 raw, full-frame writes; default `/dev/ttyUSB0`)
- **`aa-engine`** — CB session state machine (negotiate / dump / steady poll, getCAN NACK retry, dirty-reset resync) over a `Link`
- **`aa-mailbox`** — mailbox JSON message types and `RegisterBank` ↔ JSON converters (write policy, raw-hex passthrough; no WS bind)
- **`cb-daemon`** — runnable daemon: TOML/env/CLI config, engine wiring, and the axum WebSocket at `GET /v1/mailbox-stream`

## WebSocket protocol (mailbox API)

`ws://<host>:2026/v1/mailbox-stream` — JSON frames tagged by `type` (snake_case). Registers are addressed as 2-hex register ids (`"05"`); zone-bearing registers (`03`/`04`) take an optional `zone`. Units are keyed `"{unit_type}:{unit_id}"` (e.g. `"07:11111"`); client `write`/`read` default to the primary unit when `unit_type`/`unit_id` are omitted.

### Server → client

| `type` | Fields | Meaning |
|--------|--------|---------|
| `snapshot` | `units` (unit-keyed map → register id → typed payload) | Full register snapshot of all known units (on connect and after resync) |
| `event` | `unit_type`, `unit_id`, `register`, `zone?`, `payload` | Incremental register change pushed by the CB |
| `read_result` | `msg_id`, `unit_type`, `unit_id`, `register`, `zone?`, `payload` | Reply to a client `read` |
| `ack` | `msg_id`, `status` (`success` \| `error`), `reason?` | Reply to `write` / `command` |
| `status` | `state`, `detail?` | Link-state changes (`syncing`, `synced`, `link_down`, …) |
| `error` | `message`, `reason?` | Protocol / transport error not tied to a `msg_id` |

### Client → server

| `type` | Fields | Meaning |
|--------|--------|---------|
| `write` | `msg_id`, `unit_type?`, `unit_id?`, `register`, `zone?`, `payload` | Write a register (typed payload, or raw 14-char hex for unknown registers); policy-checked against read-only fields |
| `read` | `msg_id`, `unit_type?`, `unit_id?`, `register`, `zone?` | Read a register; answered with `read_result` |
| `command` | `msg_id`, `action` | Non-register command; `resync` triggers a CB register flush + fresh `snapshot` |

Example client write:

```json
{
  "type": "write",
  "msg_id": "req-101",
  "register": "05",
  "payload": { "power": "on", "mode": "cool", "target_temp_c": 23.0 }
}
```

## Configuration (`cb-daemon`)

Precedence (later wins): **defaults → TOML file → environment → CLI**.

### Config file search

When neither `--config` nor `CB_DAEMON_CONFIG` is set, the daemon looks for:

1. `/etc/cb-daemon/config.toml`
2. `./config.toml`

If neither exists, built-in defaults are used. An explicit path that is missing is an error.

Canonical example: [`packaging/cb-daemon.example.toml`](packaging/cb-daemon.example.toml).

| Field | Default | Notes |
|-------|---------|-------|
| `backend` | `mock` | `mock` \| `aoa` \| `tty` |
| `device` | unset | Absolute path when set; else AOA `/dev/usb_accessory` or TTY `/dev/ttyUSB0` |
| `bind` | `127.0.0.1:2026` | HTTP / WebSocket listen address |
| `unit_id_hint` | unset | Five hex digits; logged at startup |
| `log_level` | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` |
| `aoa_chunk_size` | `63` | ≥ 1 |
| `aoa_chunk_delay_ms` | `1` | Milliseconds between AOA chunks |
| `tty_baud` | `57600` | Must be a supported termios rate |
| `ws_idle_timeout_minutes` | `0` | Power-off failsafe disabled by default; N minutes with zero connected WebSocket clients enables it (max 1 year) |
| `ws_idle_retry_seconds` | `60` | Seconds between power-off retries while still disconnected; ≥ 1 |
| `keepalive_interval_seconds` | `30` | Seconds between daemon-initiated WebSocket pings per connected session; ≥ 1 (max 1 year) |
| `keepalive_pong_timeout_seconds` | `75` | Seconds of client silence before the session is closed; must be ≥ `keepalive_interval_seconds` (max 1 year) |
| `snapshot_timeout_seconds` | `15` | Seconds a connecting client waits for the first engine snapshot before receiving `link_down` and being closed; > 0 (max 1 year) |

The WebSocket idle failsafe is open-loop: the daemon transmits the power-off frame but cannot verify the unit obeyed. While no client is connected, the frame is re-sent every `ws_idle_retry_seconds`.

> **Development note:** with the default mock backend, an unattended daemon run with no client connected never powers off — the idle failsafe is opt-in. Set `ws_idle_timeout_minutes` to a positive value to arm it.

### Environment

`CB_DAEMON_CONFIG`, `CB_DAEMON_BACKEND`, `CB_DAEMON_DEVICE`, `CB_DAEMON_BIND`, `CB_DAEMON_LOG_LEVEL`, `CB_DAEMON_UNIT_ID_HINT`, `CB_DAEMON_AOA_CHUNK_SIZE`, `CB_DAEMON_AOA_CHUNK_DELAY_MS`, `CB_DAEMON_TTY_BAUD`, `CB_DAEMON_WS_IDLE_TIMEOUT_MINUTES`, `CB_DAEMON_WS_IDLE_RETRY_SECONDS`, `CB_DAEMON_KEEPALIVE_INTERVAL_SECONDS`, `CB_DAEMON_KEEPALIVE_PONG_TIMEOUT_SECONDS`, `CB_DAEMON_SNAPSHOT_TIMEOUT_SECONDS`

### CLI

`--config`, `--backend`, `--device`, `--bind`, `--log-level`, `--unit-id-hint`, `--aoa-chunk-size`, `--aoa-chunk-delay-ms`, `--tty-baud`, `--ws-idle-timeout-minutes`, `--ws-idle-retry-seconds`, `--keepalive-interval-seconds`, `--keepalive-pong-timeout-seconds`, `--snapshot-timeout-seconds`

### Logging

`log_level` / `CB_DAEMON_LOG_LEVEL` / `--log-level` set the tracing filter when `RUST_LOG` is unset. If `RUST_LOG` is set, it overrides those sources (standard `tracing-subscriber` `EnvFilter` behavior).

## Development

```bash
./scripts/run_codequality.sh   # fmt check + clippy (-D warnings)
./scripts/run_tests.sh         # cargo test --workspace
cargo test -p aa-crc -p aa-frame -p aa-registers -p aa-link -p aa-engine -p aa-mailbox -p cb-daemon
```

## Cross builds

```bash
./scripts/build-pi-zero.sh         # arm-unknown-linux-musleabihf via cross
./scripts/build-android-armv7.sh   # NDK / cargo-ndk (local only) → dist/android-armv7/cb-daemon
./scripts/pack-magisk.sh           # Magisk zip from packaging/magisk + that binary
```

CI builds the Pi musl target only. Android/NDK is **local only** (not wired into CI).

### Android ARMv7 (local)

Tested baseline (informative — no hard version gate in the script):

- Android NDK **r26+** or **r27**
- API **21** (`cargo-ndk --platform 21`)
- Target `armv7-linux-androideabi`
- `cargo-ndk` on `PATH`
- `ANDROID_NDK_HOME` or `ANDROID_NDK_ROOT` set

```bash
./scripts/build-android-armv7.sh   # stripped binary → dist/android-armv7/cb-daemon
./scripts/pack-magisk.sh           # → dist/android-armv7/cb-daemon-magisk.zip
```

Override binary path for packing with `CB_DAEMON_BIN=/path/to/cb-daemon`.

## Packaging

- **Magisk** (`packaging/magisk/`): installable module (flash the zip from `pack-magisk.sh`). Runtime binary/config live under `/data/adb/cb-daemon/`. Ops notes and a device start/stop checklist: [`packaging/magisk/README.md`](packaging/magisk/README.md).
- **OpenRC** (`packaging/openrc/cb-daemon`): init script for the Pi Zero W on Alpine (the systemd placeholder is retired). Install and enable steps in the template header. `/var/log` is RAM-backed (tmpfs, fstab) to spare SD card IOPS; daemon output goes to `/var/log/cb-daemon.log`, rotated with a size cap via `packaging/logrotate/cb-daemon` (copytruncate keeps the live fd; rotation runs from the 1-minute periodic — see the config header — because a slower cadence lets a debug frame flood outgrow the tmpfs and deadlock rotation; logs are intentionally volatile across reboot).
- Canonical host config example: [`packaging/cb-daemon.example.toml`](packaging/cb-daemon.example.toml).

## Deployment

Live deployment: a Raspberry Pi Zero W (Alpine Linux, 32-bit ARMv6, OpenRC) acting as the tablet-side talker on the Advantage Air Control Box (CB) RS-485 link since 2026-08-12. The wall tablet is a pure WebSocket client to the daemon.

### Hardware & wiring

- **Compute:** Raspberry Pi Zero W, powered from the CB's ~14 V feed through a 1 A slow-blow fuse and a buck converter into the PWR micro-USB.
- **Link:** CH340-based USB-to-RS485 dongle on the OTG port → `/dev/ttyUSB0`, **57600 8N1** half-duplex.
- **CB RJ45 pinout** (T568A): pin 1 green/white = RS-485 B(+), pin 2 green = RS-485 A(−), pin 4 blue = GND, pin 5 blue/white = ~14 V, pin 6 orange = GND. Identify by pin number, not colour.
- **Single tablet-side talker rule:** never run the wall tablet and the Zero W on the same RS-485 segment simultaneously.

### Link & service

- **Build:** `./scripts/build-pi-zero.sh` → static musl `cb-daemon` binary.
- **Install:** binary at `/usr/local/bin/cb-daemon`, config at `/etc/cb-daemon/config.toml` (`backend = "tty"`, `device = "/dev/ttyUSB0"`, `bind = "0.0.0.0:2026"`, optional `unit_id_hint`).
- **Service:** OpenRC init script `packaging/openrc/cb-daemon` → `/etc/init.d/cb-daemon`, `rc-update add cb-daemon default`. `supervise-daemon` respawns the child on failure (unlimited); the script waits for `/dev/ttyUSB0` at boot.
- **Northbound:** `ws://<host>:2026/v1/mailbox-stream`, single active client (second connection gets WS close `4009`).

### Logging (RAM-backed)

- `/var/log` is a **128 MB tmpfs** (fstab entry, mounted by `localmount`) to spare SD card IOPS; logs are intentionally volatile across reboot.
- `supervise-daemon` redirects child stdout/stderr to `/var/log/cb-daemon.log` (`output_log`/`error_log`); `NO_COLOR=1` strips ANSI escapes.
- Rotation is size-capped to bound RAM usage: cb-daemon 5 MB × 19 (~100 MB), messages 2 MB × 4 (~10 MB), `copytruncate` so the live fd survives. logrotate runs from the 1-minute periodic so caps are enforced promptly even during debug bursts.

### GPU memory

Headless node: `gpu_mem=32` in `/boot/config.txt` (the practical minimum with Alpine's initramfs on the legacy firmware). Note this file is regenerated on kernel upgrades, so the setting must be re-applied after `apk upgrade`.

### Rollback

Disconnect the Zero W RS-485 tap from the CB; reconnect the tablet cable path; tablet resumes USB driving (reinstall the tablet Magisk module if removed — see `packaging/magisk/README.md`). Never run both talkers on the bus.
