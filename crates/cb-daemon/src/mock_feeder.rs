//! Shared [`MockLink`] wrapper and negotiate→dump feeder.

use std::sync::Arc;
use std::time::Duration;

use aa_frame::Frame;
use aa_link::{Link, MockLink};
use aa_registers::{CanRecord, Dest, RegId, UnitId, UnitType};
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

/// Wire payloads mirrored from `aa-engine` (crate-private there).
const GET_SYSTEM_DATA: &[u8] = b"getSystemData";
const DUMP_SET_CAN: &[u8] = b"setCAN 0701000000600000000000000 ";
const EMPTY_SET_CAN: &[u8] = b"setCAN ";
const ACK_CAN: &[u8] = b"ackCAN 1";

/// Unit id used by the scripted dump sample (`abcde`).
pub(crate) const FEEDER_UNIT_ID: UnitId = match UnitId::try_new(0x0_ABCDE) {
    Ok(id) => id,
    Err(_) => UnitId::ZERO,
};

/// [`MockLink`] shared with the feeder; `read` waits when inbound is empty (not EOF).
pub(crate) struct SharedMockLink {
    inner: Arc<Mutex<MockLink>>,
    notify: Arc<Notify>,
}

impl SharedMockLink {
    /// Create a shared mock and clones for the feeder.
    #[must_use]
    pub(crate) fn new() -> (Self, Arc<Mutex<MockLink>>, Arc<Notify>) {
        let inner = Arc::new(Mutex::new(MockLink::new()));
        let notify = Arc::new(Notify::new());
        (
            Self {
                inner: Arc::clone(&inner),
                notify: Arc::clone(&notify),
            },
            inner,
            notify,
        )
    }
}

impl Link for SharedMockLink {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = {
                let mut guard = self.inner.lock().await;
                let n = guard.read(buf).await?;
                drop(guard);
                n
            };
            if n > 0 {
                return Ok(n);
            }
            self.notify.notified().await;
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut guard = self.inner.lock().await;
        guard.write_all(data).await
    }

    async fn close(&mut self) -> std::io::Result<()> {
        let result = {
            let mut guard = self.inner.lock().await;
            let result = guard.close().await;
            drop(guard);
            result
        };
        self.notify.notify_waiters();
        result
    }
}

fn encoded(payload: &[u8]) -> Vec<u8> {
    Frame {
        payload: payload.to_vec(),
    }
    .encode()
}

const fn sample_record() -> CanRecord {
    CanRecord {
        unit_type: UnitType::AIRCON,
        dest: Dest::Tablet,
        unit_id: FEEDER_UNIT_ID,
        reg: RegId::new(0x05),
        data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
    }
}

fn get_can_with_sample() -> Vec<u8> {
    let rec = sample_record();
    let mut body = b"getCAN 1 ".to_vec();
    body.extend_from_slice(rec.to_wire().as_bytes());
    body
}

async fn push_frame(mock: &Arc<Mutex<MockLink>>, notify: &Notify, payload: &[u8]) {
    let bytes = encoded(payload);
    let mut g = mock.lock().await;
    g.push_inbound(&bytes);
    drop(g);
    notify.notify_one();
}

async fn wait_written_contains(mock: &Arc<Mutex<MockLink>>, needle: &[u8]) -> bool {
    timeout(Duration::from_secs(5), async {
        loop {
            {
                let guard = mock.lock().await;
                if guard.written().windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or(false)
}

async fn take_written(mock: &Arc<Mutex<MockLink>>) -> Vec<u8> {
    let mut g = mock.lock().await;
    g.take_written()
}

/// Play negotiate→dump once, then keep the mock CB alive for steady / resync.
pub(crate) async fn run_negotiate_dump_feeder(mock: Arc<Mutex<MockLink>>, notify: Arc<Notify>) {
    if !feeder_negotiate(&mock, &notify).await {
        return;
    }
    if !feeder_dump(&mock, &notify).await {
        return;
    }
    feeder_steady_loop(mock, notify).await;
}

async fn feeder_negotiate(mock: &Arc<Mutex<MockLink>>, notify: &Notify) -> bool {
    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(GET_SYSTEM_DATA)).await {
        warn!("mock feeder: timed out waiting for getSystemData");
        return false;
    }
    let _ = take_written(mock).await;
    push_frame(mock, notify, b"CAN2 in use").await;
    true
}

async fn feeder_dump(mock: &Arc<Mutex<MockLink>>, notify: &Notify) -> bool {
    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(DUMP_SET_CAN)).await {
        warn!("mock feeder: timed out waiting for dump setCAN");
        return false;
    }
    let _ = take_written(mock).await;
    push_frame(mock, notify, &get_can_with_sample()).await;
    debug!("mock feeder: dump complete (Snapshot expected)");
    true
}

async fn feeder_steady_loop(mock: Arc<Mutex<MockLink>>, notify: Arc<Notify>) {
    loop {
        // Detect closed link without reading inbound (would corrupt queued frames).
        {
            let mut g = mock.lock().await;
            if g.write_all(&[]).await.is_err() {
                debug!("mock feeder: link closed, exiting");
                return;
            }
        }

        let _ = take_written(&mock).await;
        push_frame(&mock, &notify, b"Ping").await;
        let saw = timeout(Duration::from_secs(2), async {
            loop {
                let ready = {
                    let written = mock.lock().await.written().to_vec();
                    written_has_frame(&written, DUMP_SET_CAN)
                        || written_has_frame(&written, EMPTY_SET_CAN)
                        || written_has_frame(&written, ACK_CAN)
                        || written
                            .windows(b"<U>setCAN".len())
                            .any(|x| x == b"<U>setCAN")
                };
                if ready {
                    return true;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or(false);

        if !saw {
            sleep(Duration::from_millis(50)).await;
            continue;
        }

        let written = take_written(&mock).await;
        if written_has_frame(&written, DUMP_SET_CAN) {
            push_frame(&mock, &notify, &get_can_with_sample()).await;
        } else if written_has_frame(&written, ACK_CAN) {
            // Ack consumed; wait for next Ping cycle.
        } else {
            // EMPTY_SET_CAN or setCAN with write records → reply getCAN.
            push_frame(&mock, &notify, b"getCAN 1").await;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn written_has_frame(written: &[u8], payload: &[u8]) -> bool {
    let needle = encoded(payload);
    written.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_engine::{CbEngine, EngineCmd, EngineEvent};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn feeder_task_emits_snapshot() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (ev_tx, mut ev_rx) = mpsc::channel(32);

        let engine = tokio::spawn(async move {
            CbEngine::new(link).run(cmd_rx, ev_tx).await;
        });
        tokio::task::yield_now().await;
        let feeder = tokio::spawn(run_negotiate_dump_feeder(mock, notify));

        let snap = timeout(Duration::from_secs(5), async {
            loop {
                let ev = ev_rx.recv().await.expect("event");
                if matches!(ev, EngineEvent::Snapshot(_)) {
                    return ev;
                }
            }
        })
        .await
        .expect("snapshot timeout");
        assert!(matches!(snap, EngineEvent::Snapshot(_)));

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        let _ = timeout(Duration::from_secs(2), engine).await;
        feeder.abort();
    }
}
