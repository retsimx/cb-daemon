use super::link::{
    AOA_CONFIG_PACKET, AOA_INTER_CHUNK_DELAY, AOA_MAX_CHUNK, AccessoryTransport, AoaLinkInner,
    AoaOpenOptions, InterChunkDelay,
};
use crate::Link;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
struct FakeTransport {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug)]
struct FakeInner {
    written: Vec<Vec<u8>>,
    inbound: Vec<u8>,
    closed: bool,
    fail_write_after: Option<usize>,
    write_count: usize,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeInner {
                written: Vec::new(),
                inbound: Vec::new(),
                closed: false,
                fail_write_after: None,
                write_count: 0,
            })),
        }
    }

    fn with_inbound(bytes: &[u8]) -> Self {
        let t = Self::new();
        t.inner.lock().expect("lock").inbound = bytes.to_vec();
        t
    }

    fn fail_write_after(self, n: usize) -> Self {
        self.inner.lock().expect("lock").fail_write_after = Some(n);
        self
    }

    fn written_chunks(&self) -> Vec<Vec<u8>> {
        self.inner.lock().expect("lock").written.clone()
    }
}

impl AccessoryTransport for FakeTransport {
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
        if let Some(limit) = inner.fail_write_after
            && inner.write_count >= limit
        {
            return Err(io::Error::other("injected write failure"));
        }
        inner.write_count += 1;
        inner.written.push(data.to_vec());
        drop(inner);
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.inner.lock().expect("lock").closed = true;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RecordingDelay {
    count: Arc<Mutex<usize>>,
}

impl RecordingDelay {
    fn new() -> Self {
        Self {
            count: Arc::new(Mutex::new(0)),
        }
    }

    fn sleep_count(&self) -> usize {
        *self.count.lock().expect("lock")
    }
}

impl InterChunkDelay for RecordingDelay {
    async fn sleep(&mut self) {
        *self.count.lock().expect("lock") += 1;
    }
}

async fn open_fake(
    transport: FakeTransport,
    delay: RecordingDelay,
) -> AoaLinkInner<FakeTransport, RecordingDelay> {
    open_fake_with(transport, delay, AoaOpenOptions::default()).await
}

async fn open_fake_with(
    transport: FakeTransport,
    delay: RecordingDelay,
    opts: AoaOpenOptions,
) -> AoaLinkInner<FakeTransport, RecordingDelay> {
    AoaLinkInner::from_transport(transport, delay, opts.max_chunk)
        .await
        .expect("from_transport")
}

#[tokio::test]
async fn config_written_exactly_once_before_payload() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let mut link = open_fake(transport.clone(), delay).await;

    assert_eq!(transport.written_chunks(), vec![AOA_CONFIG_PACKET.to_vec()]);

    link.write_all(&[0xAA, 0xBB]).await.expect("payload");
    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], AOA_CONFIG_PACKET.to_vec());
    assert_eq!(chunks[1], vec![0xAA, 0xBB]);
    assert_eq!(
        chunks
            .iter()
            .filter(|c| c.as_slice() == AOA_CONFIG_PACKET)
            .count(),
        1
    );
}

#[tokio::test]
async fn config_write_failure_returns_err_no_usable_link() {
    // fail_write_after(0): first write (config packet) fails → from_transport Err
    let transport = FakeTransport::new().fail_write_after(0);
    let delay = RecordingDelay::new();
    let err = AoaLinkInner::from_transport(transport.clone(), delay, AOA_MAX_CHUNK)
        .await
        .expect_err("config write must fail");
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(
        transport.written_chunks().is_empty(),
        "no successful writes when config fails"
    );
    // transport was not wrapped in a link; caller owns/drops it — no half-open link
}

#[tokio::test]
async fn chunk_exactly_max_is_single_write_no_sleep() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;
    let payload = vec![0x42u8; AOA_MAX_CHUNK];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 2); // config + one payload chunk
    assert_eq!(chunks[1], payload);
    let num_chunks = 1usize;
    assert_eq!(delay_handle.sleep_count(), num_chunks.saturating_sub(1));
}

#[tokio::test]
async fn chunk_64_splits_with_one_sleep() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;
    let payload = vec![0x7Eu8; 64];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 3); // config + [63] + [1]
    assert_eq!(chunks[1].len(), 63);
    assert_eq!(chunks[2].len(), 1);
    assert_eq!(&chunks[1][..], &payload[..63]);
    assert_eq!(chunks[2][0], payload[63]);
    let num_chunks = 2usize;
    assert_eq!(delay_handle.sleep_count(), num_chunks.saturating_sub(1));
}

#[tokio::test]
async fn chunk_126_two_full_chunks_one_sleep() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;
    let payload = vec![0x11u8; 126];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 3); // config + [63] + [63]
    assert_eq!(chunks[1].len(), 63);
    assert_eq!(chunks[2].len(), 63);
    let num_chunks = 2usize;
    assert_eq!(delay_handle.sleep_count(), num_chunks.saturating_sub(1));
}

#[tokio::test]
async fn empty_write_ok_no_sleep_no_extra_write() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;
    let before = transport.written_chunks().len();
    link.write_all(&[]).await.expect("empty write");
    assert_eq!(transport.written_chunks().len(), before);
    let num_chunks = 0usize;
    assert_eq!(delay_handle.sleep_count(), num_chunks.saturating_sub(1));
}

#[tokio::test]
async fn delay_count_matches_saturating_sub_one() {
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;
    // 190 bytes → 4 chunks (63+63+63+1)
    let payload = vec![0xFFu8; 190];
    link.write_all(&payload).await.expect("write");
    let payload_chunks = transport.written_chunks().len() - 1; // exclude config
    assert_eq!(payload_chunks, 4);
    assert_eq!(delay_handle.sleep_count(), payload_chunks.saturating_sub(1));
}

#[tokio::test]
async fn read_and_close_match_mocklink_semantics() {
    let transport = FakeTransport::with_inbound(b"abcd");
    let delay = RecordingDelay::new();
    let mut link = open_fake(transport, delay).await;

    let mut buf = [0u8; 2];
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 2);
    assert_eq!(&buf, b"ab");
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 2);
    assert_eq!(&buf, b"cd");
    let n = link.read(&mut buf).await.expect("read");
    assert_eq!(n, 0);

    link.close().await.expect("close");
    link.close().await.expect("idempotent close");

    let read_err = link.read(&mut buf).await.expect_err("read after close");
    assert_eq!(read_err.kind(), io::ErrorKind::NotConnected);
    let write_err = link.write_all(b"x").await.expect_err("write after close");
    assert_eq!(write_err.kind(), io::ErrorKind::NotConnected);
}

#[tokio::test]
async fn write_error_stops_further_writes_and_sleeps() {
    // fail_write_after(2): config (1) + first payload chunk (2) succeed; third write fails
    let transport = FakeTransport::new().fail_write_after(2);
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;

    let payload = vec![0x55u8; 126];
    let err = link.write_all(&payload).await.expect_err("injected fail");
    assert_eq!(err.kind(), io::ErrorKind::Other);

    let chunks = transport.written_chunks();
    // config + first 63-byte chunk only; second chunk never written
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], AOA_CONFIG_PACKET.to_vec());
    assert_eq!(chunks[1].len(), 63);
    // sleep between chunks ran once before the failed second write
    assert_eq!(delay_handle.sleep_count(), 1);
}

#[tokio::test]
async fn write_error_on_first_payload_chunk_no_sleep() {
    let transport = FakeTransport::new().fail_write_after(1);
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake(transport.clone(), delay).await;

    let payload = vec![0x55u8; 126];
    let err = link.write_all(&payload).await.expect_err("fail");
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert_eq!(transport.written_chunks().len(), 1); // config only
    assert_eq!(delay_handle.sleep_count(), 0);
}

#[tokio::test]
async fn custom_max_chunk_splits_and_sleeps() {
    let opts = AoaOpenOptions {
        max_chunk: 10,
        inter_chunk_delay: Duration::from_millis(5),
    };
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake_with(transport.clone(), delay, opts).await;

    let payload = vec![0xABu8; 25];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 4); // config + [10] + [10] + [5]
    assert_eq!(chunks[1].len(), 10);
    assert_eq!(chunks[2].len(), 10);
    assert_eq!(chunks[3].len(), 5);
    assert_eq!(&chunks[1][..], &payload[..10]);
    assert_eq!(&chunks[2][..], &payload[10..20]);
    assert_eq!(&chunks[3][..], &payload[20..]);
    assert_eq!(delay_handle.sleep_count(), 2);
    assert_eq!(opts.inter_chunk_delay, Duration::from_millis(5));
    assert_ne!(opts.inter_chunk_delay, AOA_INTER_CHUNK_DELAY);
}

#[tokio::test]
async fn custom_max_chunk_exact_boundary_no_sleep() {
    let opts = AoaOpenOptions {
        max_chunk: 16,
        inter_chunk_delay: Duration::from_millis(0),
    };
    let transport = FakeTransport::new();
    let delay = RecordingDelay::new();
    let delay_handle = RecordingDelay {
        count: Arc::clone(&delay.count),
    };
    let mut link = open_fake_with(transport.clone(), delay, opts).await;

    let payload = vec![0xCDu8; 16];
    link.write_all(&payload).await.expect("write");

    let chunks = transport.written_chunks();
    assert_eq!(chunks.len(), 2); // config + one chunk
    assert_eq!(chunks[1], payload);
    assert_eq!(delay_handle.sleep_count(), 0);
}

#[test]
fn aoa_open_options_default_matches_constants() {
    let opts = AoaOpenOptions::default();
    assert_eq!(opts.max_chunk, AOA_MAX_CHUNK);
    assert_eq!(opts.inter_chunk_delay, AOA_INTER_CHUNK_DELAY);
}
