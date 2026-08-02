use std::future::Future;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{Mode, OFlags, open};
use rustix::termios::{
    ControlModes, InputModes, OptionalActions, Termios, speed, tcgetattr, tcsetattr,
};
use tokio::io::unix::AsyncFd;

use crate::{Link, closed_error};

/// Default USB-serial device path on Pi / Linux USB-RS485 adapters.
pub const TTY_DEFAULT_PATH: &str = "/dev/ttyUSB0";

/// Baud rate: 57600 (8N1 raw).
pub const TTY_BAUD: u32 = speed::B57600;

/// Options for [`TtyLink::open_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyOpenOptions {
    /// Baud rate in bits per second (e.g. 57600).
    pub baud: u32,
}

impl Default for TtyOpenOptions {
    fn default() -> Self {
        Self { baud: TTY_BAUD }
    }
}

/// Async byte transport over a serial device (or a test double).
pub(super) trait SerialTransport: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn write_all(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn close(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

/// Map a numeric baud rate to a rustix [`speed`] constant.
///
/// Supported rates include the common Linux termios speeds; at minimum 57600.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] for unsupported baud rates.
pub(super) fn map_baud_to_speed(baud: u32) -> io::Result<u32> {
    match baud {
        speed::B9600 => Ok(speed::B9600),
        speed::B19200 => Ok(speed::B19200),
        speed::B38400 => Ok(speed::B38400),
        speed::B57600 => Ok(speed::B57600),
        speed::B115200 => Ok(speed::B115200),
        speed::B230400 => Ok(speed::B230400),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported baud rate: {other}"),
        )),
    }
}

/// Apply 8N1 raw settings at `baud` to an existing [`Termios`] value.
///
/// Does not touch the FD; callers apply with [`tcsetattr`].
pub(super) fn apply_tty_settings(termios: &mut Termios, baud: u32) -> io::Result<()> {
    let speed_val = map_baud_to_speed(baud)?;
    termios.make_raw();
    termios.set_speed(speed_val)?;

    // 8N1 + enable receiver; ignore modem control lines; no hardware RTS/CTS.
    let mut control = termios.control_modes;
    control.remove(
        ControlModes::CSIZE | ControlModes::PARENB | ControlModes::CSTOPB | ControlModes::CRTSCTS,
    );
    control.insert(ControlModes::CS8 | ControlModes::CREAD | ControlModes::CLOCAL);
    termios.control_modes = control;

    // No software flow control (make_raw usually clears these; be explicit).
    let mut input = termios.input_modes;
    input.remove(InputModes::IXON | InputModes::IXOFF | InputModes::IXANY);
    termios.input_modes = input;

    Ok(())
}

fn configure_fd(fd: &OwnedFd, baud: u32) -> io::Result<()> {
    let mut termios = tcgetattr(fd).map_err(io::Error::from)?;
    apply_tty_settings(&mut termios, baud)?;
    tcsetattr(fd, OptionalActions::Now, &termios).map_err(io::Error::from)
}

/// Production transport: rustix open + termios, async I/O via [`AsyncFd`].
#[derive(Debug)]
struct FileTransport {
    fd: Option<AsyncFd<OwnedFd>>,
}

impl FileTransport {
    fn open(path: impl AsRef<Path>, baud: u32) -> io::Result<Self> {
        let fd = open(
            path.as_ref(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;

        // Termios failure must not leave a usable half-open link: drop `fd`.
        if let Err(err) = configure_fd(&fd, baud) {
            drop(fd);
            return Err(err);
        }

        let afd = AsyncFd::new(fd)?;
        Ok(Self { fd: Some(afd) })
    }
}

impl SerialTransport for FileTransport {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(afd) = self.fd.as_mut() else {
            return Err(closed_error());
        };
        loop {
            let mut guard = afd.readable_mut().await?;
            if let Ok(result) = guard.try_io(|inner| {
                rustix::io::read(inner.get_ref(), &mut *buf).map_err(io::Error::from)
            }) {
                return result;
            }
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let Some(afd) = self.fd.as_mut() else {
            return Err(closed_error());
        };
        let mut offset = 0;
        while offset < data.len() {
            let n = loop {
                let mut guard = afd.writable_mut().await?;
                if let Ok(result) = guard.try_io(|inner| {
                    rustix::io::write(inner.get_ref(), &data[offset..]).map_err(io::Error::from)
                }) {
                    break result?;
                }
            };
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            offset += n;
        }
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        // Dropping AsyncFd / OwnedFd closes the FD.
        drop(self.fd.take());
        Ok(())
    }
}

/// Generic TTY link over an injectable [`SerialTransport`]. Kept private so
/// the transport trait stays crate-internal; production callers use [`TtyLink`].
#[derive(Debug)]
pub(super) struct TtyLinkInner<T> {
    transport: Option<T>,
}

impl<T: SerialTransport> TtyLinkInner<T> {
    pub(super) const fn from_transport(transport: T) -> Self {
        Self {
            transport: Some(transport),
        }
    }
}

impl<T: SerialTransport> Link for TtyLinkInner<T> {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(transport) = self.transport.as_mut() else {
            return Err(closed_error());
        };
        transport.read(buf).await
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let Some(transport) = self.transport.as_mut() else {
            return Err(closed_error());
        };
        // Full buffer in one transport write — no artificial chunking.
        transport.write_all(data).await
    }

    async fn close(&mut self) -> io::Result<()> {
        if let Some(mut transport) = self.transport.take() {
            transport.close().await?;
        }
        Ok(())
    }
}

/// USB-serial [`Link`] over a TTY path (default [`TTY_DEFAULT_PATH`]).
///
/// Open with [`TtyLink::open`] / [`TtyLink::open_with`] / [`TtyLink::open_default`].
/// Configures 8N1 raw mode at the requested baud on open; subsequent
/// `write_all` calls write the full buffer without chunking.
#[derive(Debug)]
pub struct TtyLink {
    inner: TtyLinkInner<FileTransport>,
}

impl TtyLink {
    /// Open `path` with [`TtyOpenOptions`], apply 8N1 raw termios at `opts.baud`,
    /// and return a ready link. On termios failure the FD is dropped and an
    /// error is returned — never a half-open link.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be opened read/write, the baud rate
    /// is unsupported, termios configuration fails, or the FD cannot be
    /// registered with the reactor.
    #[allow(clippy::unused_async)] // async API parity with AoaLink::open
    pub async fn open_with(path: impl AsRef<Path>, opts: TtyOpenOptions) -> io::Result<Self> {
        let transport = FileTransport::open(path, opts.baud)?;
        Ok(Self {
            inner: TtyLinkInner::from_transport(transport),
        })
    }

    /// Open `path` with [`TtyOpenOptions::default`] (57600 8N1 raw).
    ///
    /// # Errors
    ///
    /// Same as [`TtyLink::open_with`].
    pub async fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with(path, TtyOpenOptions::default()).await
    }

    /// Open [`TTY_DEFAULT_PATH`] with default options.
    ///
    /// # Errors
    ///
    /// Same as [`TtyLink::open`].
    pub async fn open_default() -> io::Result<Self> {
        Self::open(TTY_DEFAULT_PATH).await
    }
}

impl Link for TtyLink {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).await
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.write_all(data).await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.inner.close().await
    }
}
