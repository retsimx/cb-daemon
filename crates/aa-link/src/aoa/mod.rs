//! Android USB Accessory (`/dev/usb_accessory`) [`Link`](crate::Link) backend.
//!
//! [`AoaLink`] talks to the raw accessory character device — **no termios, no
//! baud rate, no serial framing**. On open it writes a one-shot FTDI-style
//! config packet, then exposes chunked `write_all` (≤ [`AOA_MAX_CHUNK`] bytes
//! per write with [`AOA_INTER_CHUNK_DELAY`] between chunks) to match stock
//! aaservice UART pacing.
//!
//! **Exclusivity**: `aaservice` (or any other process) must **not** hold
//! `/dev/usb_accessory` open while this backend is open. Concurrent opens fail
//! or corrupt the accessory session.

mod link;

pub use link::{
    AOA_CONFIG_PACKET, AOA_DEFAULT_PATH, AOA_INTER_CHUNK_DELAY, AOA_INTER_CHUNK_DELAY_MS,
    AOA_MAX_CHUNK, AoaLink, AoaOpenOptions,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
