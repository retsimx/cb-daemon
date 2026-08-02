//! Async byte I/O seam for Android Auto link backends.
//!
//! Provides [`Link`] for engine-facing read/write/close, [`MockLink`] for
//! scripted unit tests without hardware, [`AoaLink`] for the raw
//! `/dev/usb_accessory` USB Accessory backend, and [`TtyLink`] for Linux
//! USB-serial / USB-RS485 (57600 8N1 raw).

#![allow(async_fn_in_trait)] // D3: native async `Link`; consumers use `L: Link`, not `dyn Link`.

mod aoa;
mod tty;

pub use aoa::{
    AOA_CONFIG_PACKET, AOA_DEFAULT_PATH, AOA_INTER_CHUNK_DELAY, AOA_INTER_CHUNK_DELAY_MS,
    AOA_MAX_CHUNK, AoaLink, AoaOpenOptions,
};
pub use tty::{TTY_BAUD, TTY_DEFAULT_PATH, TtyLink, TtyOpenOptions};

use std::collections::VecDeque;
use std::future::Future;
use std::io;

/// Async byte transport used by the protocol engine.
///
/// Implementors must be [`Send`]. Futures returned by I/O methods are also
/// [`Send`] so callers can drive the link on multi-thread runtimes
/// (`tokio::spawn`). Native Rust 2024 `async fn` / `impl Future` methods are not
/// dyn-compatible; consumers should use generics (`L: Link`).
pub trait Link: Send {
    /// Read up to `buf.len()` bytes into `buf`.
    ///
    /// Returns the number of bytes read. `Ok(0)` indicates end-of-stream / no
    /// more data available (for example an empty inbound script on
    /// [`MockLink`]).
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;

    /// Write all of `data` to the link.
    fn write_all(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;

    /// Close the link. Subsequent I/O should fail. Idempotent.
    fn close(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

pub(crate) fn closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "link closed")
}

/// In-memory [`Link`] with a scripted inbound queue and write capture.
///
/// No framing helpers and no auto-responses — raw bytes only.
#[derive(Debug, Default)]
pub struct MockLink {
    inbound: VecDeque<u8>,
    outbound: Vec<u8>,
    closed: bool,
}

impl MockLink {
    /// Empty mock with no scripted inbound bytes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mock preloaded with scripted inbound bytes.
    #[must_use]
    pub fn with_inbound(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            inbound: VecDeque::from(bytes.into()),
            outbound: Vec::new(),
            closed: false,
        }
    }

    /// Append bytes to the inbound script queue.
    pub fn push_inbound(&mut self, data: &[u8]) {
        self.inbound.extend(data);
    }

    /// Bytes written via [`Link::write_all`] so far.
    #[must_use]
    pub fn written(&self) -> &[u8] {
        &self.outbound
    }

    /// Drain and return the write capture buffer.
    #[must_use]
    pub fn take_written(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outbound)
    }
}

impl Link for MockLink {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Err(closed_error());
        }
        if self.inbound.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(self.inbound.len());
        for slot in &mut buf[..n] {
            // inbound non-empty and n <= len; pop_front always yields Some.
            let Some(byte) = self.inbound.pop_front() else {
                break;
            };
            *slot = byte;
        }
        Ok(n)
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.closed {
            return Err(closed_error());
        }
        self.outbound.extend_from_slice(data);
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.closed = true;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Link, MockLink};

    #[tokio::test]
    async fn write_all_captures_via_written_and_take_written() {
        let mut link = MockLink::new();
        link.write_all(b"hello").await.expect("write");
        assert_eq!(link.written(), b"hello");
        link.write_all(b" world").await.expect("write");
        assert_eq!(link.written(), b"hello world");
        let taken = link.take_written();
        assert_eq!(taken, b"hello world");
        assert_eq!(link.written(), b"");
    }

    #[tokio::test]
    async fn scripted_inbound_partial_reads() {
        let mut link = MockLink::with_inbound(b"abcdef");
        let mut buf = [0u8; 3];
        let n = link.read(&mut buf).await.expect("read");
        assert_eq!(n, 3);
        assert_eq!(&buf, b"abc");
        let n = link.read(&mut buf).await.expect("read");
        assert_eq!(n, 3);
        assert_eq!(&buf, b"def");
        let n = link.read(&mut buf).await.expect("read");
        assert_eq!(n, 0);

        link.push_inbound(b"xy");
        let mut small = [0u8; 1];
        let n = link.read(&mut small).await.expect("read");
        assert_eq!(n, 1);
        assert_eq!(small[0], b'x');
        let n = link.read(&mut small).await.expect("read");
        assert_eq!(n, 1);
        assert_eq!(small[0], b'y');
    }

    #[tokio::test]
    async fn empty_inbound_returns_ok_zero() {
        let mut link = MockLink::new();
        let mut buf = [0u8; 8];
        let n = link.read(&mut buf).await.expect("read");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn io_after_close_errors() {
        let mut link = MockLink::with_inbound(b"data");
        link.write_all(b"out").await.expect("write before close");
        link.close().await.expect("close");
        // close is idempotent
        link.close().await.expect("second close");

        let mut buf = [0u8; 4];
        let read_err = link.read(&mut buf).await.expect_err("read after close");
        assert_eq!(read_err.kind(), std::io::ErrorKind::NotConnected);

        let write_err = link
            .write_all(b"more")
            .await
            .expect_err("write after close");
        assert_eq!(write_err.kind(), std::io::ErrorKind::NotConnected);
    }
}
