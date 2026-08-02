use super::link::{
    SerialTransport, TTY_BAUD, TtyLinkInner, TtyOpenOptions, apply_tty_settings, map_baud_to_speed,
};
use crate::Link;
use rustix::fs::{Mode, OFlags, open};
use rustix::termios::{ControlModes, InputModes, speed, tcgetattr};
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct FakeTransport {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug)]
struct FakeInner {
    written: Vec<Vec<u8>>,
    inbound: Vec<u8>,
    closed: bool,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeInner {
                written: Vec::new(),
                inbound: Vec::new(),
                closed: false,
            })),
        }
    }

    fn with_inbound(bytes: &[u8]) -> Self {
        let t = Self::new();
        t.inner.lock().expect("lock").inbound = bytes.to_vec();
        t
    }

    fn written_chunks(&self) -> Vec<Vec<u8>> {
        self.inner.lock().expect("lock").written.clone()
    }
}

impl SerialTransport for FakeTransport {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().expect("lock");
        if inner.closed {
            return Err(crate::closed_error());
        }
        if inner.inbound.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(inner.inbound.len());
        buf[..n].copy_from_slice(&inner.inbound[..n]);
        inner.inbound.drain(..n);
        drop(inner);
        Ok(n)
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let mut inner = self.inner.lock().expect("lock");
        if inner.closed {
            return Err(crate::closed_error());
        }
        inner.written.push(data.to_vec());
        drop(inner);
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.inner.lock().expect("lock").closed = true;
        Ok(())
    }
}

fn open_fake(transport: FakeTransport) -> TtyLinkInner<FakeTransport> {
    TtyLinkInner::from_transport(transport)
}

#[tokio::test]
async fn write_all_sends_full_buffer_as_single_chunk() {
    let transport = FakeTransport::new();
    let mut link = open_fake(transport.clone());
    let payload = vec![0xAAu8; 200];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 1, "must not chunk writes");
    assert_eq!(chunks[0], payload);
}

#[tokio::test]
async fn write_all_empty_is_single_empty_transport_write() {
    let transport = FakeTransport::new();
    let mut link = open_fake(transport.clone());
    link.write_all(&[]).await.expect("empty write");
    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_empty());
}

#[tokio::test]
async fn partial_and_multi_reads() {
    let transport = FakeTransport::with_inbound(b"abcdef");
    let mut link = open_fake(transport);

    let mut buf = [0u8; 2];
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 2);
    assert_eq!(&buf, b"ab");
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 2);
    assert_eq!(&buf, b"cd");
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 2);
    assert_eq!(&buf, b"ef");
}

#[tokio::test]
async fn empty_inbound_returns_ok_zero() {
    let transport = FakeTransport::new();
    let mut link = open_fake(transport);
    let mut buf = [0u8; 8];
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn close_idempotent_and_io_after_close_not_connected() {
    let transport = FakeTransport::with_inbound(b"xy");
    let mut link = open_fake(transport);

    link.close().await.expect("close");
    link.close().await.expect("idempotent close");

    let mut buf = [0u8; 2];
    let read_err = link.read(&mut buf).await.expect_err("read after close");
    assert_eq!(read_err.kind(), io::ErrorKind::NotConnected);
    let write_err = link.write_all(b"z").await.expect_err("write after close");
    assert_eq!(write_err.kind(), io::ErrorKind::NotConnected);
}

#[test]
fn apply_tty_settings_57600_8n1_raw_on_ptmx() {
    let Ok(fd) = open(
        "/dev/ptmx",
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return; // optional helper test; skip if no ptmx
    };
    let mut termios = tcgetattr(&fd).expect("tcgetattr");
    apply_tty_settings(&mut termios, TTY_BAUD).expect("apply");

    assert_eq!(termios.input_speed(), TTY_BAUD);
    assert_eq!(termios.output_speed(), TTY_BAUD);
    assert!(termios.control_modes.contains(ControlModes::CS8));
    assert!(termios.control_modes.contains(ControlModes::CREAD));
    assert!(termios.control_modes.contains(ControlModes::CLOCAL));
    assert!(!termios.control_modes.contains(ControlModes::PARENB));
    assert!(!termios.control_modes.contains(ControlModes::CSTOPB));
    assert!(!termios.input_modes.contains(InputModes::IXON));
    assert!(!termios.input_modes.contains(InputModes::IXOFF));
}

#[test]
fn map_baud_57600_to_rustix_speed() {
    assert_eq!(map_baud_to_speed(57600).expect("57600"), speed::B57600);
    assert_eq!(
        map_baud_to_speed(TTY_BAUD).expect("TTY_BAUD"),
        speed::B57600
    );
}

#[test]
fn map_baud_rejects_unsupported_with_clear_error() {
    let err = map_baud_to_speed(12345).expect_err("unsupported");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let msg = err.to_string();
    assert!(
        msg.contains("unsupported baud") && msg.contains("12345"),
        "clear message, got: {msg}"
    );
}

#[test]
fn apply_tty_settings_rejects_unsupported_baud() {
    let Ok(fd) = open(
        "/dev/ptmx",
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return;
    };
    let mut termios = tcgetattr(&fd).expect("tcgetattr");
    let err = apply_tty_settings(&mut termios, 999).expect_err("bad baud");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("unsupported baud"));
}

#[test]
fn apply_tty_settings_115200_on_ptmx() {
    let Ok(fd) = open(
        "/dev/ptmx",
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return;
    };
    let mut termios = tcgetattr(&fd).expect("tcgetattr");
    apply_tty_settings(&mut termios, 115_200).expect("apply 115200");
    assert_eq!(termios.input_speed(), speed::B115200);
    assert_eq!(termios.output_speed(), speed::B115200);
}

#[test]
fn tty_open_options_default_is_57600() {
    let opts = TtyOpenOptions::default();
    assert_eq!(opts.baud, 57600);
    assert_eq!(opts.baud, TTY_BAUD);
}
