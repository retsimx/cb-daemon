//! Sync CB session state machine (no I/O).

use aa_registers::{CanRecord, RegisterBank};

use crate::event::{EngineCmd, EngineEvent};
use crate::wire::{
    ACK_CAN, DUMP_SET_CAN, EMPTY_SET_CAN, GET_SYSTEM_DATA, build_set_can, is_can2_in_use,
    is_get_can, parse_get_can,
};

/// Protocol phase for the tablet-side CB session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Init,
    WaitPing,
    Negotiate,
    ExpectCan2,
    RequestDump,
    Steady,
}

/// Pure sync session: Ping/frame handlers, register bank, write queue.
#[derive(Debug)]
pub(crate) struct Session {
    state: State,
    bank: RegisterBank,
    write_queue: Vec<CanRecord>,
    ack_armed: bool,
    dump_sent: bool,
    shutdown: bool,
}

impl Session {
    /// Create a session in [`State::Init`].
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            state: State::Init,
            bank: RegisterBank::new(),
            write_queue: Vec::new(),
            ack_armed: false,
            dump_sent: false,
            shutdown: false,
        }
    }

    /// Whether [`EngineCmd::Shutdown`] was applied.
    #[must_use]
    pub(crate) const fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Apply an inbound engine command.
    pub(crate) fn apply_cmd(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::WriteRegisters(records) => {
                self.write_queue.extend(records);
            }
            EngineCmd::ResyncMailbox => {
                self.state = State::RequestDump;
                self.dump_sent = false;
                self.ack_armed = false;
            }
            EngineCmd::Shutdown => {
                self.shutdown = true;
            }
        }
    }

    /// Handle a Ping: return at most one outbound **payload** (not framed).
    pub(crate) fn on_ping(&mut self) -> Option<Vec<u8>> {
        if self.state == State::Init {
            self.state = State::WaitPing;
        }
        match self.state {
            State::Init | State::WaitPing => {
                self.state = State::Negotiate;
                Some(GET_SYSTEM_DATA.to_vec())
            }
            State::Negotiate => {
                // Re-enter expect phase; stay silent until CAN2 reply.
                self.state = State::ExpectCan2;
                None
            }
            State::ExpectCan2 => None,
            State::RequestDump => {
                self.dump_sent = true;
                Some(DUMP_SET_CAN.to_vec())
            }
            State::Steady => Some(self.steady_tx()),
        }
    }

    /// Handle a non-Ping frame payload; returns zero or more engine events.
    pub(crate) fn on_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        match self.state {
            State::Init | State::WaitPing => {
                vec![EngineEvent::ProtocolWarn(
                    "frame before negotiate started".into(),
                )]
            }
            State::Negotiate | State::ExpectCan2 => self.on_negotiate_frame(payload),
            State::RequestDump => self.on_dump_frame(payload),
            State::Steady => self.on_steady_frame(payload),
        }
    }

    fn steady_tx(&mut self) -> Vec<u8> {
        if self.ack_armed {
            self.ack_armed = false;
            return ACK_CAN.to_vec();
        }
        if !self.write_queue.is_empty() {
            let records = std::mem::take(&mut self.write_queue);
            return build_set_can(&records);
        }
        EMPTY_SET_CAN.to_vec()
    }

    fn on_negotiate_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        if is_can2_in_use(payload) {
            let detail = String::from_utf8_lossy(payload).into_owned();
            self.state = State::RequestDump;
            self.dump_sent = false;
            vec![EngineEvent::Negotiated { detail }]
        } else {
            let detail = format!("negotiate mismatch: {}", String::from_utf8_lossy(payload));
            vec![EngineEvent::ProtocolWarn(detail)]
        }
    }

    fn on_dump_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        if !is_get_can(payload) {
            return vec![EngineEvent::ProtocolWarn(format!(
                "unexpected frame during dump: {}",
                String::from_utf8_lossy(payload)
            ))];
        }
        match parse_get_can(payload) {
            Ok(records) => {
                for record in &records {
                    self.bank.apply(record);
                }
                // Early getCAN before dump TX: keep RequestDump until dump is sent.
                if !self.dump_sent {
                    return Vec::new();
                }
                self.state = State::Steady;
                self.ack_armed = false;
                vec![EngineEvent::Snapshot(self.bank.clone())]
            }
            Err(err) => vec![EngineEvent::ProtocolWarn(format!(
                "getCAN parse failed during dump: {err:?}"
            ))],
        }
    }

    fn on_steady_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        if !is_get_can(payload) {
            return vec![EngineEvent::ProtocolWarn(format!(
                "unexpected steady frame: {}",
                String::from_utf8_lossy(payload)
            ))];
        }
        match parse_get_can(payload) {
            Ok(records) => {
                for record in &records {
                    self.bank.apply(record);
                }
                self.ack_armed = true;
                vec![EngineEvent::RegistersChanged { records }]
            }
            Err(err) => vec![EngineEvent::ProtocolWarn(format!(
                "getCAN parse failed: {err:?}"
            ))],
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_registers::{Dest, RegId, UnitId, UnitType};

    fn sample_record() -> CanRecord {
        CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        }
    }

    fn get_can_payload(records: &[CanRecord]) -> Vec<u8> {
        let mut s = String::from("getCAN 1");
        for r in records {
            s.push(' ');
            s.push_str(&r.to_wire());
        }
        s.into_bytes()
    }

    #[test]
    fn negotiate_then_dump_path() {
        let mut s = Session::new();
        assert_eq!(s.state, State::Init);

        let tx = s.on_ping().expect("getSystemData");
        assert_eq!(tx, GET_SYSTEM_DATA);
        assert_eq!(s.state, State::Negotiate);

        let ev = s.on_frame(b"CAN2 in use");
        assert!(matches!(ev.as_slice(), [EngineEvent::Negotiated { .. }]));
        assert_eq!(s.state, State::RequestDump);

        let dump = s.on_ping().expect("dump");
        assert_eq!(dump, DUMP_SET_CAN);

        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(ev.as_slice(), [EngineEvent::Snapshot(_)]));
        assert_eq!(s.state, State::Steady);
        assert_eq!(
            s.bank.get(rec.unit_type, rec.unit_id, rec.reg),
            Some(rec.data)
        );
    }

    #[test]
    fn negotiate_mismatch_warns_and_stays_alive() {
        let mut s = Session::new();
        let _ = s.on_ping();
        let ev = s.on_frame(b"CAN1 weird");
        assert!(matches!(ev.as_slice(), [EngineEvent::ProtocolWarn(_)]));
        assert!(matches!(s.state, State::Negotiate | State::ExpectCan2));
        // Still alive: another ping does not panic; eventual CAN2 works.
        assert!(s.on_ping().is_none());
        assert_eq!(s.state, State::ExpectCan2);
        let ev = s.on_frame(b"status: CAN2 in use ok");
        assert!(matches!(ev.as_slice(), [EngineEvent::Negotiated { .. }]));
        assert_eq!(s.state, State::RequestDump);
    }

    #[test]
    fn steady_priority_ack_then_queue_then_empty() {
        let mut s = Session::new();
        // Fast-forward to Steady.
        let _ = s.on_ping();
        let _ = s.on_frame(b"CAN2 in use");
        let _ = s.on_ping();
        let _ = s.on_frame(b"getCAN 1");
        assert_eq!(s.state, State::Steady);

        // Empty poll when nothing pending.
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);

        // Queue a write — next ping drains it.
        let rec = CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::ControlBox,
            unit_id: UnitId::try_new(0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        s.apply_cmd(EngineCmd::WriteRegisters(vec![rec.clone()]));
        let tx = s.on_ping().unwrap();
        assert_eq!(tx, build_set_can(std::slice::from_ref(&rec)));

        // Arm ack via getCAN; ack beats a newly queued write.
        let _ = s.on_frame(&get_can_payload(&[sample_record()]));
        s.apply_cmd(EngineCmd::WriteRegisters(vec![rec]));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        // After ack, queued write drains.
        let tx = s.on_ping().unwrap();
        assert!(tx.starts_with(b"setCAN "));
        assert_ne!(tx, EMPTY_SET_CAN);
        assert_ne!(tx, ACK_CAN);
    }

    #[test]
    fn write_get_can_arms_ack() {
        let mut s = Session::new();
        let _ = s.on_ping();
        let _ = s.on_frame(b"CAN2 in use");
        let _ = s.on_ping();
        let _ = s.on_frame(b"getCAN 1");

        let rec = sample_record();
        let mut write = rec.clone();
        write.dest = Dest::ControlBox;
        s.apply_cmd(EngineCmd::WriteRegisters(vec![write]));
        let _ = s.on_ping(); // setCAN write
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
    }

    #[test]
    fn resync_reenters_dump_and_snapshot() {
        let mut s = Session::new();
        let _ = s.on_ping();
        let _ = s.on_frame(b"CAN2 in use");
        let _ = s.on_ping();
        let _ = s.on_frame(b"getCAN 1");
        assert_eq!(s.state, State::Steady);

        s.apply_cmd(EngineCmd::ResyncMailbox);
        assert_eq!(s.state, State::RequestDump);

        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(ev.as_slice(), [EngineEvent::Snapshot(_)]));
        assert_eq!(s.state, State::Steady);
    }

    #[test]
    fn early_get_can_before_dump_tx_applies_but_stays() {
        let mut s = Session::new();
        let _ = s.on_ping();
        let _ = s.on_frame(b"CAN2 in use");
        assert_eq!(s.state, State::RequestDump);
        assert!(!s.dump_sent);

        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(ev.is_empty());
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(
            s.bank.get(rec.unit_type, rec.unit_id, rec.reg),
            Some(rec.data)
        );

        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let ev = s.on_frame(b"getCAN 1");
        assert!(matches!(ev.as_slice(), [EngineEvent::Snapshot(_)]));
        assert_eq!(s.state, State::Steady);
    }

    #[test]
    fn shutdown_flag() {
        let mut s = Session::new();
        assert!(!s.is_shutdown());
        s.apply_cmd(EngineCmd::Shutdown);
        assert!(s.is_shutdown());
    }
}
