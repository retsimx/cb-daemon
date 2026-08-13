//! Shared [`MockLink`] wrapper and negotiate→dump feeder.

use std::sync::Arc;
use std::time::Duration;

use aa_frame::{Frame, FrameScanner};
use aa_link::{Link, MockLink};
use aa_registers::{CanRecord, Dest, RegId, UnitId, UnitType};
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

/// Wire payloads mirrored from `aa-engine` (crate-private there).
const DIRTY_RESET_SET_CAN: &[u8] = b"setCAN 0701000000600000000000000";
const DUMP_SET_CAN: &[u8] = b"setCAN 0801000000600000000000000 0801000000236000000000000";
const EMPTY_SET_CAN: &[u8] = b"setCAN ";
const ACK_CAN: &[u8] = b"ackCAN 1";

/// Unit id used by the scripted dump sample (`abcde`).
pub(crate) const FEEDER_UNIT_ID: UnitId = match UnitId::try_new(0x0_ABCDE) {
    Ok(id) => id,
    Err(_) => UnitId::ZERO,
};

/// Scripted behavior for the mock feeder.
///
/// [`FeederSpec::default`] reproduces the legacy feeder exactly; acceptance
/// tests override individual behaviors with the builder methods.
#[derive(Clone, Default)]
pub struct FeederSpec {
    omit_reg05: bool,
    scripted_reg06: Option<CanRecord>,
    stop_after_dump: bool,
}

impl FeederSpec {
    /// Omit the 07/reg-05 sample from every getCAN the feeder produces
    /// (dirty-reset reply, dump reply, flush full-set reply); the 08/reg-06
    /// sample is kept, so the engine's register bank never sees reg 05.
    #[allow(dead_code)]
    #[must_use]
    pub const fn without_reg05(mut self) -> Self {
        self.omit_reg05 = true;
        self
    }

    /// Inject one unsolicited reg-06 getCAN record on the next steady poll,
    /// triggering the JZ18 reg-07 handshake (engine queues `build_jz18`).
    #[allow(dead_code)]
    #[must_use]
    pub const fn scripted_reg06(
        mut self,
        unit_type: UnitType,
        unit_id: UnitId,
        data: [u8; 7],
    ) -> Self {
        self.scripted_reg06 = Some(CanRecord {
            unit_type,
            dest: Dest::Tablet,
            unit_id,
            reg: RegId::new(0x06),
            data,
        });
        self
    }

    /// Stop after negotiate+dump instead of entering the steady loop: the bus
    /// goes silent (no frames, no events) while the engine stays alive — the
    /// D-12 silent-bus repro.
    #[allow(dead_code)]
    #[must_use]
    pub const fn stop_after_dump(mut self) -> Self {
        self.stop_after_dump = true;
        self
    }

    /// Sample records carried by getCAN replies (dump and flush full-set).
    fn samples(&self) -> Vec<CanRecord> {
        let mut out = Vec::new();
        if !self.omit_reg05 {
            out.push(sample_record());
        }
        out.push(sample_record_08());
        out
    }
}

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

const fn sample_record_08() -> CanRecord {
    CanRecord {
        unit_type: UnitType::new(0x08),
        dest: Dest::Tablet,
        unit_id: FEEDER_UNIT_ID,
        reg: RegId::new(0x06),
        data: [0; 7],
    }
}

/// Unknown-register (16) record carried only by the flush full-set reply, so
/// reads of an unknown register resolve to a raw-hex `read_result` (D-5).
const fn sample_record_16() -> CanRecord {
    CanRecord {
        unit_type: UnitType::AIRCON,
        dest: Dest::Tablet,
        unit_id: FEEDER_UNIT_ID,
        reg: RegId::new(0x16),
        data: [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03],
    }
}

fn get_can_with_sample(spec: &FeederSpec) -> Vec<u8> {
    get_can_from_records(&spec.samples())
}

/// A reg-06 `ControlBox` flush: `ControlBox`-destined record with register `0x06`
/// and all-zero data. The engine queues these to pull a unit's full register
/// set (the read verb's unit-scoped flush and the missing-reg-05 recovery);
/// real CBs answer a flush with the full set, so the mock replies likewise.
/// JZ18 reg-07 replies and regular writes never match (reg `0x06` only).
fn is_reg06_flush(record: &CanRecord) -> bool {
    record.dest == Dest::ControlBox && record.reg == RegId::new(0x06) && record.data == [0; 7]
}

/// Parse the records carried by the first `setCAN …` frame in `written`.
fn parse_set_can_records(written: &[u8]) -> Option<Vec<CanRecord>> {
    let frames = FrameScanner::new().push(written).ok()?;
    for frame in frames {
        if let Some(body) = frame.payload.strip_prefix(b"setCAN ") {
            return CanRecord::parse_many(std::str::from_utf8(body).ok()?).ok();
        }
    }
    None
}

/// Build `getCAN 1 …` carrying `records` (empty when there are none).
fn get_can_from_records(records: &[CanRecord]) -> Vec<u8> {
    let mut body = b"getCAN 1 ".to_vec();
    for rec in records {
        body.extend_from_slice(rec.to_wire().as_bytes());
        body.push(b' ');
    }
    body.pop();
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
/// With [`FeederSpec::stop_after_dump`], return right after the dump: the bus
/// goes silent while the engine stays alive (the D-12 silent-bus repro).
pub(crate) async fn run_feeder(mock: Arc<Mutex<MockLink>>, notify: Arc<Notify>, spec: FeederSpec) {
    if !feeder_negotiate(&mock, &notify).await {
        return;
    }
    if !feeder_dump(&mock, &notify, &spec).await {
        return;
    }
    if spec.stop_after_dump {
        debug!("mock feeder: stop-after-dump, bus goes silent");
        return;
    }
    feeder_steady_loop(mock, notify, spec).await;
}

/// Play negotiate→dump once, then keep the mock CB alive for steady / resync.
///
/// Equivalent to [`run_feeder`] with [`FeederSpec::default`].
#[allow(dead_code)]
pub(crate) async fn run_negotiate_dump_feeder(mock: Arc<Mutex<MockLink>>, notify: Arc<Notify>) {
    run_feeder(mock, notify, FeederSpec::default()).await;
}

async fn feeder_negotiate(mock: &Arc<Mutex<MockLink>>, notify: &Notify) -> bool {
    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(b"getSystemData")).await {
        warn!("mock feeder: timed out waiting for getSystemData");
        return false;
    }
    let _ = take_written(mock).await;
    push_frame(mock, notify, b"CAN2 in use").await;
    true
}

async fn feeder_dump(mock: &Arc<Mutex<MockLink>>, notify: &Notify, spec: &FeederSpec) -> bool {
    // Two-phase dump: reg-06 dirty reset → reset getCAN → ack → 08-flush dump.
    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(DIRTY_RESET_SET_CAN)).await {
        warn!("mock feeder: timed out waiting for dirty-reset setCAN");
        return false;
    }
    let _ = take_written(mock).await;
    push_frame(mock, notify, &get_can_with_sample(spec)).await;

    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(ACK_CAN)).await {
        warn!("mock feeder: timed out waiting for reset ack");
        return false;
    }
    let _ = take_written(mock).await;

    push_frame(mock, notify, b"Ping").await;
    if !wait_written_contains(mock, &encoded(DUMP_SET_CAN)).await {
        warn!("mock feeder: timed out waiting for dump setCAN");
        return false;
    }
    let _ = take_written(mock).await;
    push_frame(mock, notify, &get_can_with_sample(spec)).await;
    debug!("mock feeder: dump complete (Snapshot expected)");
    true
}

/// Reply to one polled `written` batch. Dispatch order matters: engine-internal
/// setCAN payloads (dump/resync, empty poll) are matched before generic
/// record parsing so they are never treated as write echoes.
///
/// `scripted` holds the next unsolicited reg-06 record to announce: the
/// first bare-getCAN poll (empty poll with no pending echoes) carries it
/// instead of an empty getCAN, firing it exactly once.
#[allow(clippy::too_many_arguments)]
async fn reply_to_poll(
    mock: &Arc<Mutex<MockLink>>,
    notify: &Notify,
    written: Vec<u8>,
    pending_echo: &mut Vec<CanRecord>,
    spec: &FeederSpec,
    scripted: &mut Option<CanRecord>,
) {
    if written_has_frame(&written, DUMP_SET_CAN) {
        push_frame(mock, notify, &get_can_with_sample(spec)).await;
    } else if written_has_frame(&written, ACK_CAN) {
        // Ack consumed; wait for next Ping cycle.
    } else if written_has_frame(&written, EMPTY_SET_CAN) {
        // Empty poll: report applied-but-unreported registers (write echo),
        // else a bare getCAN request (or the scripted reg-06 announcement).
        if let Some(record) = scripted.take() {
            push_frame(mock, notify, &get_can_from_records(&[record])).await;
        } else if pending_echo.is_empty() {
            push_frame(mock, notify, b"getCAN 1").await;
        } else {
            push_frame(mock, notify, &get_can_from_records(pending_echo)).await;
            pending_echo.clear();
        }
    } else if let Some(records) = parse_set_can_records(&written) {
        // setCAN carrying write records → the CB applies the write and
        // reports the updated registers back in its next getCAN
        // (aaservice parity); queue the echo so RegistersChanged fan-out
        // events reach WebSocket sessions after the write's ack.
        //
        // A reg-06 ControlBox flush flips the CB into full-set reply mode:
        // the getCAN carries the whole sample register set plus every flush
        // record from the batch (mirrors real CB flush behavior and lets
        // read-verb pending reads resolve against reg 05). The full-set
        // reply wins over write echoes for that batch; all flush records
        // found are included, in batch order.
        let flush_records: Vec<CanRecord> = records
            .iter()
            .filter(|record| is_reg06_flush(record))
            .cloned()
            .collect();
        if flush_records.is_empty() {
            pending_echo.extend(records);
        } else {
            let mut reply = spec.samples();
            reply.push(sample_record_16());
            reply.extend(flush_records);
            push_frame(mock, notify, &get_can_from_records(&reply)).await;
        }
    }
}

async fn feeder_steady_loop(mock: Arc<Mutex<MockLink>>, notify: Arc<Notify>, spec: FeederSpec) {
    // Register records the CB has applied but not yet reported (write echo,
    // deferred until the next empty poll so the WriteFlushed ack — sent on
    // TX — always precedes the RegistersChanged fan-out event).
    let mut pending_echo: Vec<CanRecord> = Vec::new();
    let mut spec = spec;
    let mut scripted = spec.scripted_reg06.take();
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

        reply_to_poll(
            &mock,
            &notify,
            take_written(&mock).await,
            &mut pending_echo,
            &spec,
            &mut scripted,
        )
        .await;
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

    fn flush_record(unit_type: UnitType) -> CanRecord {
        CanRecord {
            unit_type,
            dest: Dest::ControlBox,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x06),
            data: [0; 7],
        }
    }

    fn set_can_payload(records: &[CanRecord]) -> Vec<u8> {
        let mut body = b"setCAN ".to_vec();
        for rec in records {
            body.extend_from_slice(rec.to_wire().as_bytes());
            body.push(b' ');
        }
        body.pop();
        body
    }

    /// One polled `written` batch exactly as `feeder_steady_loop` sees it:
    /// full encoded frames on the wire.
    fn written_batch(records: &[CanRecord]) -> Vec<u8> {
        encoded(&set_can_payload(records))
    }

    /// Drain the frame `reply_to_poll` pushed into the mock's inbound.
    async fn take_inbound_payload(mock: &Arc<Mutex<MockLink>>) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];
        let mut g = mock.lock().await;
        let n = g.read(&mut buf).await.expect("inbound read");
        drop(g);
        FrameScanner::new()
            .push(&buf[..n])
            .expect("frame scan")
            .into_iter()
            .next()
            .map(|frame| frame.payload)
            .expect("reply frame")
    }

    async fn parse_reply_records(mock: &Arc<Mutex<MockLink>>) -> Vec<CanRecord> {
        let payload = take_inbound_payload(mock).await;
        let body = payload.strip_prefix(b"getCAN 1 ").expect("getCAN reply");
        CanRecord::parse_many(std::str::from_utf8(body).expect("utf8")).expect("records")
    }

    #[tokio::test]
    async fn flush_set_can_replies_with_full_sample_set() {
        let (_, mock, notify) = SharedMockLink::new();
        let aircon_flush = flush_record(UnitType::AIRCON);
        let e08_flush = flush_record(UnitType::new(0x08));
        let write = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        };
        let mut pending_echo = Vec::new();
        let mut scripted = None;

        // Batch with two flush records + a regular write: the full-set reply
        // wins, carrying the sample set and every flush record in batch order.
        reply_to_poll(
            &mock,
            &notify,
            written_batch(&[aircon_flush.clone(), e08_flush.clone(), write]),
            &mut pending_echo,
            &FeederSpec::default(),
            &mut scripted,
        )
        .await;

        assert_eq!(
            parse_reply_records(&mock).await,
            vec![
                sample_record(),
                sample_record_08(),
                sample_record_16(),
                aircon_flush,
                e08_flush
            ],
            "flush batch must reply with the full sample set plus the flush records"
        );
        assert!(pending_echo.is_empty(), "flush batch must not defer echoes");
    }

    #[tokio::test]
    async fn non_flush_set_can_keeps_echo_behavior() {
        let (_, mock, notify) = SharedMockLink::new();
        let write = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        };
        let jz18 = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::ControlBox,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x07),
            data: [0; 7],
        };
        let mut pending_echo = Vec::new();
        let mut scripted = None;

        reply_to_poll(
            &mock,
            &notify,
            written_batch(&[write.clone(), jz18.clone()]),
            &mut pending_echo,
            &FeederSpec::default(),
            &mut scripted,
        )
        .await;

        assert_eq!(
            pending_echo,
            vec![write, jz18],
            "writes and JZ18 reg-07 replies must still echo"
        );
        let mut buf = vec![0u8; 64];
        let n = mock
            .lock()
            .await
            .read(&mut buf)
            .await
            .expect("inbound read");
        assert_eq!(n, 0, "non-flush batch must not push an immediate reply");
    }

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
                if matches!(ev, EngineEvent::Snapshot { .. }) {
                    return ev;
                }
            }
        })
        .await
        .expect("snapshot timeout");
        assert!(matches!(snap, EngineEvent::Snapshot { .. }));

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        let _ = timeout(Duration::from_secs(2), engine).await;
        feeder.abort();
    }

    #[tokio::test]
    async fn scripted_reg06_fires_on_next_empty_poll() {
        let (_, mock, notify) = SharedMockLink::new();
        let scripted = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let mut pending_echo = Vec::new();
        let mut scripted_slot = Some(scripted.clone());

        // Empty poll with no pending echoes: the bare getCAN slot carries the
        // scripted reg-06 announcement exactly once.
        reply_to_poll(
            &mock,
            &notify,
            encoded(EMPTY_SET_CAN),
            &mut pending_echo,
            &FeederSpec::default(),
            &mut scripted_slot,
        )
        .await;

        assert_eq!(
            parse_reply_records(&mock).await,
            vec![scripted],
            "empty poll must announce the scripted reg-06 record"
        );
        assert!(
            scripted_slot.is_none(),
            "scripted reg-06 must fire exactly once"
        );
    }

    #[tokio::test]
    async fn scripted_reg06_wins_over_pending_echoes_on_next_empty_poll() {
        let (_, mock, notify) = SharedMockLink::new();
        let write = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        };
        let scripted = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: FEEDER_UNIT_ID,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let mut pending_echo = vec![write.clone()];
        let mut scripted_slot = Some(scripted.clone());

        reply_to_poll(
            &mock,
            &notify,
            encoded(EMPTY_SET_CAN),
            &mut pending_echo,
            &FeederSpec::default(),
            &mut scripted_slot,
        )
        .await;

        assert_eq!(
            parse_reply_records(&mock).await,
            vec![scripted],
            "the scripted announcement must fire on the next empty poll"
        );
        assert!(
            scripted_slot.is_none(),
            "scripted reg-06 must fire exactly once"
        );
        assert_eq!(
            pending_echo,
            vec![write],
            "pending write echoes must stay queued for a later poll"
        );
    }

    #[tokio::test]
    async fn without_reg05_omits_sample_from_flush_full_set_reply() {
        let (_, mock, notify) = SharedMockLink::new();
        let aircon_flush = flush_record(UnitType::AIRCON);
        let mut pending_echo = Vec::new();
        let mut scripted = None;

        reply_to_poll(
            &mock,
            &notify,
            written_batch(std::slice::from_ref(&aircon_flush)),
            &mut pending_echo,
            &FeederSpec::default().without_reg05(),
            &mut scripted,
        )
        .await;

        let records = parse_reply_records(&mock).await;
        assert!(
            records.iter().all(|rec| rec.reg != RegId::new(0x05)),
            "without_reg05 full-set reply must not carry reg 05: {records:?}"
        );
        assert!(
            records
                .iter()
                .any(|rec| rec.reg == RegId::new(0x06) && rec.unit_type == UnitType::new(0x08)),
            "without_reg05 full-set reply must keep the 08/reg-06 sample: {records:?}"
        );
        assert!(records.contains(&aircon_flush));
    }

    #[tokio::test]
    async fn without_reg05_snapshot_lacks_reg05_sample() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (ev_tx, mut ev_rx) = mpsc::channel(32);

        let engine = tokio::spawn(async move {
            CbEngine::new(link).run(cmd_rx, ev_tx).await;
        });
        tokio::task::yield_now().await;
        let spec = FeederSpec::default().without_reg05();
        let feeder = tokio::spawn(run_feeder(mock, notify, spec));

        let snap = timeout(Duration::from_secs(5), async {
            loop {
                let ev = ev_rx.recv().await.expect("event");
                if matches!(ev, EngineEvent::Snapshot { .. }) {
                    return ev;
                }
            }
        })
        .await
        .expect("snapshot timeout");
        let EngineEvent::Snapshot { bank, .. } = snap else {
            panic!("expected snapshot");
        };
        assert!(
            bank.records_for_unit(UnitType::AIRCON, FEEDER_UNIT_ID)
                .iter()
                .all(|rec| rec.reg != RegId::new(0x05)),
            "bank must never see reg 05 under without_reg05"
        );
        assert!(
            bank.has_unit(UnitType::new(0x08), FEEDER_UNIT_ID),
            "08/reg-06 sample must still be in the bank"
        );

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        let _ = timeout(Duration::from_secs(2), engine).await;
        feeder.abort();
    }
}
