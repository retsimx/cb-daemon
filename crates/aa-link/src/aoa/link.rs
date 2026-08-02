use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Link, closed_error};

/// Default accessory character device path on Magisk/root wall tablets.
pub const AOA_DEFAULT_PATH: &str = "/dev/usb_accessory";

/// FTDI-style config packet written once, unchunked, on successful open.
pub const AOA_CONFIG_PACKET: [u8; 8] = [0x00, 0xE1, 0x00, 0x00, 0x08, 0x01, 0x00, 0x00];

/// Maximum payload bytes per underlying write after open.
pub const AOA_MAX_CHUNK: usize = 63;

/// Delay inserted between successive payload chunks (not after the last).
pub const AOA_INTER_CHUNK_DELAY: Duration = Duration::from_millis(1);

/// Async byte transport over the accessory device (or a test double).
pub(super) trait AccessoryTransport: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn write_all(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn close(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

/// Injectable delay between payload chunks.
pub(super) trait InterChunkDelay: Send {
    fn sleep(&mut self) -> impl Future<Output = ()> + Send;
}

/// Production delay: [`tokio::time::sleep`] for [`AOA_INTER_CHUNK_DELAY`].
#[derive(Debug, Default, Clone, Copy)]
struct TokioDelay;

impl InterChunkDelay for TokioDelay {
    async fn sleep(&mut self) {
        tokio::time::sleep(AOA_INTER_CHUNK_DELAY).await;
    }
}

/// Production transport wrapping a read/write [`File`] on the accessory path.
#[derive(Debug)]
struct FileTransport {
    file: Option<File>,
}

impl FileTransport {
    async fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(path)
            .await?;
        Ok(Self { file: Some(file) })
    }
}

impl AccessoryTransport for FileTransport {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(file) = self.file.as_mut() else {
            return Err(closed_error());
        };
        file.read(buf).await
    }

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(closed_error());
        };
        file.write_all(data).await
    }

    async fn close(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
            // Dropping the File closes the FD.
            drop(file);
        }
        Ok(())
    }
}

/// Generic accessory link (transport + delay). Kept private so traits stay
/// crate-internal; production callers use [`AoaLink`].
#[derive(Debug)]
pub(super) struct AoaLinkInner<T, D> {
    transport: Option<T>,
    delay: D,
}

impl<T, D> AoaLinkInner<T, D>
where
    T: AccessoryTransport,
    D: InterChunkDelay,
{
    /// Build from an already-open transport that has **not** yet had the config
    /// packet written. Writes [`AOA_CONFIG_PACKET`] once unchunked.
    pub(super) async fn from_transport(mut transport: T, delay: D) -> io::Result<Self> {
        write_config_packet(&mut transport).await?;
        Ok(Self {
            transport: Some(transport),
            delay,
        })
    }
}

async fn write_config_packet<T: AccessoryTransport>(transport: &mut T) -> io::Result<()> {
    transport.write_all(&AOA_CONFIG_PACKET).await
}

impl<T, D> Link for AoaLinkInner<T, D>
where
    T: AccessoryTransport,
    D: InterChunkDelay,
{
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
        if data.is_empty() {
            return Ok(());
        }
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + AOA_MAX_CHUNK).min(data.len());
            transport.write_all(&data[offset..end]).await?;
            offset = end;
            if offset < data.len() {
                self.delay.sleep().await;
            }
        }
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        if let Some(mut transport) = self.transport.take() {
            transport.close().await?;
        }
        Ok(())
    }
}

/// USB Accessory [`Link`] over `/dev/usb_accessory` (or a custom path).
///
/// Open with [`AoaLink::open`] / [`AoaLink::open_default`]. Config packet is
/// written once on open; subsequent `write_all` calls are chunked.
#[derive(Debug)]
pub struct AoaLink {
    inner: AoaLinkInner<FileTransport, TokioDelay>,
}

impl AoaLink {
    /// Open `path` read/write (no create), write [`AOA_CONFIG_PACKET`] once
    /// unchunked, and return a ready link. On config write failure the FD is
    /// dropped and an error is returned — never a half-open link.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be opened read/write or if the
    /// config packet write fails.
    pub async fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let transport = FileTransport::open(path).await?;
        let inner = AoaLinkInner::from_transport(transport, TokioDelay).await?;
        Ok(Self { inner })
    }

    /// Open [`AOA_DEFAULT_PATH`].
    ///
    /// # Errors
    ///
    /// Same as [`AoaLink::open`].
    pub async fn open_default() -> io::Result<Self> {
        Self::open(AOA_DEFAULT_PATH).await
    }
}

impl Link for AoaLink {
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
