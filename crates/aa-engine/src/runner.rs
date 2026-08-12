//! Async CB engine runner over a [`Link`].

use aa_frame::{Frame, FrameScanner};
use aa_link::Link;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::event::{EngineCmd, EngineEvent, SessionState};
use crate::session::Session;
use crate::wire::is_ping;

/// Async engine that drives a [`Link`] through a sync [`Session`].
pub struct CbEngine<L: Link> {
    link: L,
}

impl<L: Link> CbEngine<L> {
    /// Wrap an open link. Session state starts at Init.
    #[must_use]
    pub const fn new(link: L) -> Self {
        Self { link }
    }

    /// Run until [`EngineCmd::Shutdown`], link EOF (`Ok(0)`), or a link I/O error.
    ///
    /// Half-duplex: transmits at most one framed payload per parsed Ping, and
    /// only as the reply to that Ping.
    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<EngineCmd>,
        ev_tx: mpsc::Sender<EngineEvent>,
    ) {
        let mut session = Session::new();
        let mut scanner = FrameScanner::new();
        let mut buf = [0u8; 1024];

        let _ = ev_tx
            .send(EngineEvent::SessionState(SessionState::Negotiating))
            .await;

        loop {
            if session.is_shutdown() {
                let _ = self.link.close().await;
                return;
            }

            tokio::select! {
                cmd = cmd_rx.recv() => {
                    if Self::apply_recv_cmd(&mut session, cmd, &ev_tx).await {
                        let _ = self.link.close().await;
                        return;
                    }
                }
                result = self.link.read(&mut buf) => {
                    if self
                        .on_link_read(&mut session, &mut scanner, &mut cmd_rx, &ev_tx, result, &buf)
                        .await
                    {
                        return;
                    }
                }
            }
        }
    }

    /// Apply a command from `recv`. Returns `true` when the runner should exit.
    async fn apply_recv_cmd(
        session: &mut Session,
        cmd: Option<EngineCmd>,
        ev_tx: &mpsc::Sender<EngineEvent>,
    ) -> bool {
        let Some(cmd) = cmd else {
            return true;
        };
        if matches!(cmd, EngineCmd::ResyncMailbox) {
            let _ = ev_tx
                .send(EngineEvent::SessionState(SessionState::Resyncing))
                .await;
        }
        session.apply_cmd(cmd);
        session.is_shutdown()
    }

    /// Handle one `link.read` result. Returns `true` when the runner should exit.
    async fn on_link_read(
        &mut self,
        session: &mut Session,
        scanner: &mut FrameScanner,
        cmd_rx: &mut mpsc::Receiver<EngineCmd>,
        ev_tx: &mpsc::Sender<EngineEvent>,
        result: std::io::Result<usize>,
        buf: &[u8],
    ) -> bool {
        let n = match result {
            Ok(0) => {
                let _ = ev_tx
                    .send(EngineEvent::SessionState(SessionState::LinkDown))
                    .await;
                let _ = ev_tx
                    .send(EngineEvent::LinkError(
                        "link EOF (read returned 0); accessory closed or detached".into(),
                    ))
                    .await;
                let _ = self.link.close().await;
                return true;
            }
            Ok(n) => n,
            Err(err) => {
                let _ = ev_tx
                    .send(EngineEvent::SessionState(SessionState::LinkDown))
                    .await;
                let _ = ev_tx.send(EngineEvent::LinkError(err.to_string())).await;
                let _ = self.link.close().await;
                return true;
            }
        };

        let frames = match scanner.push(&buf[..n]) {
            Ok(frames) => frames,
            Err(err) => {
                // CRC-failed getCAN frames must arm ackCAN 0 on the next ping
                // (aaservice parity) so the CB retries the records.
                if let aa_frame::FrameError::InvalidCrc { payload, .. } = &err
                    && payload.starts_with(b"getCAN")
                {
                    session.set_crc_ok(false);
                }
                let _ = ev_tx
                    .send(EngineEvent::ProtocolWarn(format!("frame scan: {err:?}")))
                    .await;
                return false;
            }
        };

        for frame in frames {
            if self.dispatch_frame(session, cmd_rx, ev_tx, frame).await {
                return true;
            }
        }
        false
    }

    /// Process one parsed frame (drain cmds, Ping TX or event fan-out).
    /// Returns `true` when the runner should exit.
    async fn dispatch_frame(
        &mut self,
        session: &mut Session,
        cmd_rx: &mut mpsc::Receiver<EngineCmd>,
        ev_tx: &mpsc::Sender<EngineEvent>,
        frame: Frame,
    ) -> bool {
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, EngineCmd::ResyncMailbox) {
                let _ = ev_tx
                    .send(EngineEvent::SessionState(SessionState::Resyncing))
                    .await;
            }
            session.apply_cmd(cmd);
        }
        if session.is_shutdown() {
            let _ = self.link.close().await;
            return true;
        }

        if is_ping(&frame.payload) {
            if let Some(payload) = session.on_ping() {
                let was_write = session.take_write_flushed();
                let encoded = Frame { payload }.encode();
                debug!(frame = %String::from_utf8_lossy(&encoded), "engine TX");
                if let Err(err) = self.link.write_all(&encoded).await {
                    let _ = ev_tx
                        .send(EngineEvent::SessionState(SessionState::LinkDown))
                        .await;
                    let _ = ev_tx.send(EngineEvent::LinkError(err.to_string())).await;
                    let _ = self.link.close().await;
                    return true;
                }
                if was_write && ev_tx.send(EngineEvent::WriteFlushed).await.is_err() {
                    warn!("engine event channel closed; engine exiting");
                    let _ = self.link.close().await;
                    return true;
                }
            }
            return false;
        }

        debug!(frame = %String::from_utf8_lossy(&frame.payload), "engine RX");

        for event in session.on_frame(&frame.payload) {
            let is_snapshot = matches!(event, EngineEvent::Snapshot { .. });
            if ev_tx.send(event).await.is_err() {
                warn!("engine event channel closed; engine exiting");
                let _ = self.link.close().await;
                return true;
            }
            if is_snapshot {
                let _ = ev_tx
                    .send(EngineEvent::SessionState(SessionState::Synced))
                    .await;
            }
        }
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use aa_frame::Frame;
    use aa_link::{Link, MockLink};
    use aa_registers::{CanRecord, Dest, RegId, UnitId, UnitType};
    use tokio::sync::{Mutex, Notify, mpsc};
    use tokio::time::{Duration, timeout};

    use super::CbEngine;
    use crate::event::{EngineCmd, EngineEvent, SessionState};
    use crate::wire::{ACK_CAN, DIRTY_RESET_SET_CAN, DUMP_SET_CAN, EMPTY_SET_CAN, GET_SYSTEM_DATA};

    /// [`MockLink`] shared with tests; `read` waits when inbound is empty (not EOF).
    struct SharedMockLink {
        inner: Arc<Mutex<MockLink>>,
        notify: Arc<Notify>,
    }

    impl SharedMockLink {
        fn new() -> (Self, Arc<Mutex<MockLink>>, Arc<Notify>) {
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

    async fn push_frame(mock: &Arc<Mutex<MockLink>>, notify: &Notify, payload: &[u8]) {
        let bytes = Frame {
            payload: payload.to_vec(),
        }
        .encode();
        let mut g = mock.lock().await;
        g.push_inbound(&bytes);
        drop(g);
        notify.notify_one();
    }

    fn sample_record() -> CanRecord {
        CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        }
    }

    async fn wait_written_contains(mock: &Arc<Mutex<MockLink>>, needle: &[u8]) {
        timeout(Duration::from_secs(2), async {
            loop {
                {
                    let guard = mock.lock().await;
                    if guard.written().windows(needle.len()).any(|w| w == needle) {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timeout waiting for written bytes");
    }

    #[allow(clippy::future_not_send)] // test helper; closure is not stored across tasks
    async fn recv_event(
        ev_rx: &mut mpsc::Receiver<EngineEvent>,
        pred: impl Fn(&EngineEvent) -> bool,
    ) -> EngineEvent {
        timeout(Duration::from_secs(2), async {
            loop {
                let ev = ev_rx.recv().await.expect("event");
                if pred(&ev) {
                    return ev;
                }
            }
        })
        .await
        .expect("timeout waiting for event")
    }

    fn encoded(payload: &[u8]) -> Vec<u8> {
        Frame {
            payload: payload.to_vec(),
        }
        .encode()
    }

    async fn negotiate_through_can2(mock: &Arc<Mutex<MockLink>>, notify: &Notify) {
        push_frame(mock, notify, b"Ping").await;
        wait_written_contains(mock, &encoded(GET_SYSTEM_DATA)).await;
        push_frame(mock, notify, b"CAN2 in use").await;
    }

    fn get_can_body(records: &[CanRecord]) -> Vec<u8> {
        let mut body = b"getCAN 1 ".to_vec();
        for rec in records {
            body.extend_from_slice(rec.to_wire().as_bytes());
            body.push(b' ');
        }
        body.pop();
        body
    }

    /// Two-phase dump: dirty reset → reset getCAN → ack → 08-flush dump.
    async fn dump_via_two_phase(mock: &Arc<Mutex<MockLink>>, notify: &Notify) {
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(mock, notify, b"Ping").await;
        wait_written_contains(mock, &encoded(DIRTY_RESET_SET_CAN)).await;
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        let reset_rec = CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        push_frame(mock, notify, &get_can_body(&[reset_rec])).await;
        push_frame(mock, notify, b"Ping").await;
        wait_written_contains(mock, &encoded(ACK_CAN)).await;
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(mock, notify, b"Ping").await;
        wait_written_contains(mock, &encoded(DUMP_SET_CAN)).await;
    }

    // Single end-to-end acceptance scenario (negotiate→dump→poll→write→ack);
    // kept as one test despite length >50 for readability of the path.
    #[tokio::test]
    async fn mocklink_full_path_negotiate_dump_poll_write_ack() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        negotiate_through_can2(&mock, &notify).await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Negotiated { .. })).await;

        dump_via_two_phase(&mock, &notify).await;

        {
            let rec = sample_record();
            let mut body = b"getCAN 1 ".to_vec();
            body.extend_from_slice(rec.to_wire().as_bytes());
            push_frame(&mock, &notify, &body).await;
        }
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(ACK_CAN)).await;

        // The reset-response getCAN announced reg 06, so JZ18 precedes the poll.
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        let jz18 =
            crate::wire::build_jz18(UnitType::new(0x07), UnitId::try_new(0x0_ABCDE).unwrap());
        let expected_jz18 = encoded(&crate::wire::build_set_can(std::slice::from_ref(&jz18)));
        wait_written_contains(&mock, &expected_jz18).await;

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(EMPTY_SET_CAN)).await;

        let write_rec = CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::ControlBox,
            unit_id: UnitId::try_new(0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        cmd_tx
            .send(EngineCmd::WriteRegisters(vec![write_rec.clone()]))
            .await
            .unwrap();

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        let expected_set = encoded(&crate::wire::build_set_can(std::slice::from_ref(
            &write_rec,
        )));
        wait_written_contains(&mock, &expected_set).await;

        {
            let mut body = b"getCAN 1 ".to_vec();
            body.extend_from_slice(sample_record().to_wire().as_bytes());
            push_frame(&mock, &notify, &body).await;
        }
        let _ = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::RegistersChanged { .. })
        })
        .await;

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(ACK_CAN)).await;

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn mocklink_read_register_resolves_on_flush_get_can() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        negotiate_through_can2(&mock, &notify).await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Negotiated { .. })).await;

        dump_via_two_phase(&mock, &notify).await;
        let rec = sample_record();
        push_frame(&mock, &notify, &get_can_body(std::slice::from_ref(&rec))).await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(ACK_CAN)).await;

        // The reset-response getCAN announced reg 06, so JZ18 precedes the poll.
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        let jz18 =
            crate::wire::build_jz18(UnitType::new(0x07), UnitId::try_new(0x0_ABCDE).unwrap());
        let expected_jz18 = encoded(&crate::wire::build_set_can(std::slice::from_ref(&jz18)));
        wait_written_contains(&mock, &expected_jz18).await;

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(EMPTY_SET_CAN)).await;

        // Queue a read of reg 05 on the announced unit.
        cmd_tx
            .send(EngineCmd::ReadRegister {
                unit_type: UnitType::new(0x07),
                unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
                reg: RegId::new(0x05),
                zone: None,
            })
            .await
            .unwrap();

        // Next ping TXs the unit-scoped reg-06 flush (same record shape as the
        // write-path test's flush).
        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        let flush = CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::ControlBox,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let expected_flush = encoded(&crate::wire::build_set_can(std::slice::from_ref(&flush)));
        wait_written_contains(&mock, &expected_flush).await;

        // The flush's getCAN carries a fresh reg-05 payload → RegisterRead.
        let mut fresh = rec.clone();
        fresh.data = [0x02, 0x02, 0x04, 0x31, 0x00, 0x02, 0x00];
        push_frame(&mock, &notify, &get_can_body(&[fresh.clone()])).await;
        let read = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::RegisterRead { .. })
        })
        .await;
        match read {
            EngineEvent::RegisterRead {
                unit_type,
                unit_id,
                reg,
                zone,
                data: Some(data),
            } => {
                assert_eq!(unit_type, fresh.unit_type);
                assert_eq!(unit_id, fresh.unit_id);
                assert_eq!(reg, fresh.reg);
                assert_eq!(zone, None);
                assert_eq!(data, fresh.data);
            }
            other => panic!("expected resolved RegisterRead, got {other:?}"),
        }

        {
            let mut g = mock.lock().await;
            let _ = g.take_written();
        }
        push_frame(&mock, &notify, b"Ping").await;
        wait_written_contains(&mock, &encoded(ACK_CAN)).await;

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn mocklink_resync_emits_fresh_snapshot() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        negotiate_through_can2(&mock, &notify).await;
        dump_via_two_phase(&mock, &notify).await;
        push_frame(&mock, &notify, b"getCAN 1").await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;

        cmd_tx.send(EngineCmd::ResyncMailbox).await.unwrap();
        dump_via_two_phase(&mock, &notify).await;
        push_frame(&mock, &notify, b"getCAN 1 0703abcde0501010330000100").await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn link_error_emits_and_exits() {
        let mut link = MockLink::new();
        link.close().await.unwrap();

        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(1);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        let ev = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::LinkError(_))).await;
        assert!(matches!(ev, EngineEvent::LinkError(_)));

        timeout(Duration::from_secs(2), join)
            .await
            .expect("join")
            .expect("task");
    }

    #[tokio::test]
    async fn link_eof_zero_read_emits_link_error_and_exits() {
        // Regression: accessory detach returned Ok(0) and the engine exited silently.
        let link = MockLink::new(); // empty inbound → read returns Ok(0)

        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(1);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        let ev = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::LinkError(_))).await;
        match ev {
            EngineEvent::LinkError(msg) => {
                assert!(
                    msg.contains("EOF") || msg.contains('0'),
                    "expected EOF wording, got {msg}"
                );
            }
            other => panic!("expected LinkError, got {other:?}"),
        }

        timeout(Duration::from_secs(2), join)
            .await
            .expect("join")
            .expect("task");
    }

    #[tokio::test]
    async fn session_state_first_event_is_negotiating() {
        let (link, _mock, _notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        let first = timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("timeout waiting for first event")
            .expect("event channel closed");
        assert!(
            matches!(first, EngineEvent::SessionState(SessionState::Negotiating)),
            "first event should be SessionState(Negotiating), got {first:?}"
        );

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn session_state_synced_accompanies_snapshot() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        negotiate_through_can2(&mock, &notify).await;
        dump_via_two_phase(&mock, &notify).await;
        push_frame(&mock, &notify, b"getCAN 1").await;
        let snap = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;
        assert!(matches!(snap, EngineEvent::Snapshot { .. }));
        let next = timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("timeout waiting for Synced after Snapshot")
            .expect("event channel closed");
        assert!(
            matches!(next, EngineEvent::SessionState(SessionState::Synced)),
            "expected SessionState(Synced) right after Snapshot, got {next:?}"
        );

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn session_state_resync_emits_resyncing_before_synced_snapshot() {
        let (link, mock, notify) = SharedMockLink::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        negotiate_through_can2(&mock, &notify).await;
        dump_via_two_phase(&mock, &notify).await;
        push_frame(&mock, &notify, b"getCAN 1").await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;
        let _ = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::SessionState(SessionState::Synced))
        })
        .await;

        cmd_tx.send(EngineCmd::ResyncMailbox).await.unwrap();
        let resyncing = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::SessionState(SessionState::Resyncing))
        })
        .await;
        assert!(matches!(
            resyncing,
            EngineEvent::SessionState(SessionState::Resyncing)
        ));

        dump_via_two_phase(&mock, &notify).await;
        push_frame(&mock, &notify, b"getCAN 1").await;
        let _ = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::Snapshot { .. })).await;
        let synced = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::SessionState(SessionState::Synced))
        })
        .await;
        assert!(matches!(
            synced,
            EngineEvent::SessionState(SessionState::Synced)
        ));

        cmd_tx.send(EngineCmd::Shutdown).await.unwrap();
        timeout(Duration::from_secs(2), join)
            .await
            .expect("engine join timeout")
            .expect("engine task");
    }

    #[tokio::test]
    async fn session_state_link_down_before_link_error() {
        let mut link = MockLink::new();
        link.close().await.unwrap();

        let (_cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(1);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);

        let engine = CbEngine::new(link);
        let join = tokio::spawn(async move {
            engine.run(cmd_rx, ev_tx).await;
        });

        let state = recv_event(&mut ev_rx, |e| {
            matches!(e, EngineEvent::SessionState(SessionState::LinkDown))
        })
        .await;
        assert!(matches!(
            state,
            EngineEvent::SessionState(SessionState::LinkDown)
        ));
        let err = recv_event(&mut ev_rx, |e| matches!(e, EngineEvent::LinkError(_))).await;
        assert!(matches!(err, EngineEvent::LinkError(_)));

        timeout(Duration::from_secs(2), join)
            .await
            .expect("join")
            .expect("task");
    }
}
