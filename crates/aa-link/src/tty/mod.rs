//! USB-serial / USB-RS485 [`Link`](crate::Link) backend (`TtyLink`).
//!
//! Targets Linux hosts (e.g. Pi Zero W) talking to an Advantage Air control
//! box through a USB-RS485 adapter exposed as a TTY (default
//! [`TTY_DEFAULT_PATH`] `/dev/ttyUSB0`).
//!
//! ## Framing
//!
//! - **Baud**: [`TTY_BAUD`] (57600)
//! - **Data**: 8 data bits, no parity, 1 stop bit (8N1)
//! - **Mode**: raw (non-canonical, no echo, no software flow control)
//!
//! ## Writes
//!
//! `write_all` sends the **full** buffer in one transport write (no artificial
//! chunking, no inter-chunk delay). The USB-RS485 adapter is assumed to handle
//! RS485 direction switching; **GPIO DE/RE control is out of scope** for v1.
//!
//! Backend selection for the daemon is build-time / packaging — this module
//! does not provide a runtime backend enum.

mod link;

pub use link::{TTY_BAUD, TTY_DEFAULT_PATH, TtyLink, TtyOpenOptions};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
