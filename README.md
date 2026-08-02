# cb-daemon

Foundation for a Control Box (CB) mailbox sync daemon that talks to Advantage Air air-conditioning systems. This repository holds the pure protocol crates (CRC, framing, registers, link backends, session engine) and engineering scaffold; the mailbox layer and full packaging come later.

## Workspace

Current Cargo workspace members:

- **`aa-crc`** — CRC-8 used on CB frames
- **`aa-frame`** — `<U>…</U=xx>` frame encode/decode and burst scanning
- **`aa-registers`** — register IDs, CAN2 wire codec, and register bank (scaffold)
- **`aa-link`** — async byte I/O seam (`Link`), `MockLink` for hardware-free tests, `AoaLink` for raw `/dev/usb_accessory` (config on open, chunked writes; aaservice must not hold the accessory while open), and `TtyLink` for Linux USB-serial / USB-RS485 (57600 8N1 raw, full-frame writes; default `/dev/ttyUSB0`)
- **`aa-engine`** — CB session state machine (negotiate / dump / steady poll) over a `Link`
- **`aa-mailbox`** — northbound mailbox JSON message types and `RegisterBank` ↔ JSON converters (no WS bind)
- **`cb-daemon`** — runnable daemon: TOML/env/CLI config, engine wiring, and single-session axum WebSocket at `GET /v1/mailbox-stream`

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

### Environment

`CB_DAEMON_CONFIG`, `CB_DAEMON_BACKEND`, `CB_DAEMON_DEVICE`, `CB_DAEMON_BIND`, `CB_DAEMON_LOG_LEVEL`, `CB_DAEMON_UNIT_ID_HINT`, `CB_DAEMON_AOA_CHUNK_SIZE`, `CB_DAEMON_AOA_CHUNK_DELAY_MS`, `CB_DAEMON_TTY_BAUD`

### CLI

`--config`, `--backend`, `--device`, `--bind`, `--log-level`, `--unit-id-hint`, `--aoa-chunk-size`, `--aoa-chunk-delay-ms`, `--tty-baud`

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
./scripts/build-android-armv7.sh   # NDK / cargo-ndk (local only)
```

CI builds the Pi musl target only; Android/NDK is local scaffolding for later Magisk work.

## Packaging stubs

`packaging/magisk/` and `packaging/systemd/` are placeholders — not installable Magisk modules or production unit files yet. Ship `packaging/cb-daemon.example.toml` as the config template.

## Tracking

Epic / design work for this scaffold: [GitHub issue #2](https://github.com/retsimx/cb-daemon/issues/2).
