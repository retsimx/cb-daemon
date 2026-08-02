# cb-daemon

Foundation for a Control Box (CB) mailbox sync daemon that talks to Advantage Air air-conditioning systems. This repository holds the pure protocol crates and engineering scaffold; link backends, the sync engine, and full packaging come later.

## Workspace

Current Cargo workspace members:

- **`aa-crc`** — CRC-8 used on CB frames
- **`aa-frame`** — `<U>…</U=xx>` frame encode/decode and burst scanning
- **`aa-registers`** — register IDs, CAN2 wire codec, and register bank (scaffold)

More crates (`aa-link`, `aa-engine`, `aa-mailbox`, `cb-daemon`, …) will join the workspace in later issues.

## Development

```bash
./scripts/run_codequality.sh   # fmt check + clippy (-D warnings)
./scripts/run_tests.sh         # cargo test --workspace
cargo test -p aa-crc -p aa-frame -p aa-registers
```

## Cross builds

```bash
./scripts/build-pi-zero.sh         # arm-unknown-linux-musleabihf via cross
./scripts/build-android-armv7.sh   # NDK / cargo-ndk (local only)
```

CI builds the Pi musl target only; Android/NDK is local scaffolding for later Magisk work.

## Packaging stubs

`packaging/magisk/` and `packaging/systemd/` are placeholders — not installable Magisk modules or production unit files yet.

## Tracking

Epic / design work for this scaffold: [GitHub issue #2](https://github.com/retsimx/cb-daemon/issues/2).
