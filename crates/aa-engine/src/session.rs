//! Sync CB session state machine (no I/O).

use aa_registers::{CanRecord, Dest, RegId, RegisterBank, UnitId, UnitType};

use crate::event::{EngineCmd, EngineEvent};
use crate::wire::{
    ACK_CAN, ACK_CAN_ZERO, DIRTY_RESET_SET_CAN, DUMP_SET_CAN, EMPTY_SET_CAN, GET_SYSTEM_DATA,
    build_jz18, build_set_can, is_can2_in_use, is_get_can, is_get_can_nack, parse_get_can,
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
#[allow(clippy::struct_excessive_bools)] // protocol flags are independent, not a bitfield
pub(crate) struct Session {
    state: State,
    bank: RegisterBank,
    write_queue: Vec<CanRecord>,
    /// Reads awaiting resolution by their flush's `getCAN` (see [`PendingRead`]).
    pending_reads: Vec<PendingRead>,
    ack_armed: bool,
    dump_sent: bool,
    /// After a dump `getCAN` NACK, resend dump once the ack has been flushed.
    dump_needs_resend: bool,
    /// Reg-06 zero-uid flush (`DIRTY_RESET_SET_CAN`) sent this dump cycle; resets
    /// the CB dirty flag so the following flush returns the full register set.
    dirty_reset_sent: bool,
    /// NACKs on the dirty-reset setCAN; bounded so an uncooperative CB cannot
    /// livelock the dump (mirrors aaservice's ≤3 CAN retries).
    reset_nacks: u8,
    /// Set when `steady_tx` drained the write queue; consumed by the runner to
    /// emit [`EngineEvent::WriteFlushed`] after the frame is transmitted.
    write_flushed: bool,
    /// Last complete non-Ping frame's CRC outcome (OEM `f4170w`): valid frames
    /// set 1, CRC-failed frames set 0; feeds the outbound `ackCAN 0|1` polarity.
    crc_ok: bool,
    /// Mirrors stock `canInUse`: after `CAN2 in use`, skip empty `setCAN`.
    can_in_use: bool,
    shutdown: bool,
}

/// A register read awaiting its flush's `getCAN`.
#[derive(Debug)]
pub(crate) struct PendingRead {
    unit_type: UnitType,
    unit_id: UnitId,
    reg: RegId,
    zone: Option<u8>,
    /// Set when `steady_tx` drained the write queue carrying the flush; only
    /// then may a `getCAN` resolve this read (a spontaneous pre-flush `getCAN`
    /// must not answer a read with stale pre-flush data).
    flush_sent: bool,
}

impl Session {
    /// Create a session in [`State::Init`].
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            state: State::Init,
            bank: RegisterBank::new(),
            write_queue: Vec::new(),
            pending_reads: Vec::new(),
            ack_armed: false,
            dump_sent: false,
            dump_needs_resend: false,
            dirty_reset_sent: false,
            reset_nacks: 0,
            write_flushed: false,
            crc_ok: true,
            can_in_use: false,
            shutdown: false,
        }
    }

    /// Whether [`EngineCmd::Shutdown`] was applied.
    #[must_use]
    pub(crate) const fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Consume the write-flushed flag (set when `steady_tx` drained the write
    /// queue); the runner emits [`EngineEvent::WriteFlushed`] after TX.
    #[must_use]
    pub(crate) fn take_write_flushed(&mut self) -> bool {
        std::mem::take(&mut self.write_flushed)
    }

    /// Record a CRC-failed inbound frame (OEM `f4170w`): polarity drops to 0;
    /// a getCAN failure additionally arms the ack latch (OEM `f4169v`) so the
    /// next ping carries `ackCAN 0` even without a later good frame.
    pub(crate) const fn on_crc_failure(&mut self, is_get_can: bool) {
        self.crc_ok = false;
        if is_get_can {
            self.ack_armed = true;
        }
    }

    /// Apply an inbound engine command.
    pub(crate) fn apply_cmd(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::WriteRegisters(records) => {
                self.write_queue.extend(records);
            }
            EngineCmd::ReadRegister {
                unit_type,
                unit_id,
                reg,
                zone,
            } => {
                // Unit-scoped reg-06 flush pulls the register set from the CB
                // (same pattern as the missing-reg-05 unit flush); the flush's
                // getCAN is what the read is resolved against. Dedupe an
                // identical flush already queued (same target + reg 06 + zero
                // data) so concurrent reads of one unit cost one flush.
                let flush = CanRecord {
                    unit_type,
                    dest: Dest::ControlBox,
                    unit_id,
                    reg: RegId::new(0x06),
                    data: [0; 7],
                };
                let already_queued = self.write_queue.iter().any(|queued| {
                    queued.unit_type == flush.unit_type
                        && queued.unit_id == flush.unit_id
                        && queued.reg == flush.reg
                        && queued.data == flush.data
                });
                if !already_queued {
                    self.write_queue.push(flush);
                }
                self.pending_reads.push(PendingRead {
                    unit_type,
                    unit_id,
                    reg,
                    zone,
                    flush_sent: false,
                });
            }
            EngineCmd::ResyncMailbox => {
                self.state = State::RequestDump;
                self.dump_sent = false;
                self.dump_needs_resend = false;
                self.dirty_reset_sent = false;
                self.reset_nacks = 0;
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
            State::Negotiate | State::ExpectCan2 => {
                // Stock poll: re-TX getSystemData every ping until CAN2 arrives.
                // Going silent after one attempt hangs forever if that frame is lost
                // (live bring-up: no CAN2, then accessory EIO).
                self.state = State::ExpectCan2;
                Some(GET_SYSTEM_DATA.to_vec())
            }
            State::RequestDump => {
                // Ack after a dump getCAN NACK / success / reset response must beat
                // re-sending the dump.
                if self.ack_armed {
                    return Some(self.steady_tx());
                }
                // Phase 1: reg-06 zero-uid flush resets the CB dirty flag so the CB
                // re-sends the full register set (aa_interop spec). Without it the
                // following flush only returns unsent/changed registers and MyAir5
                // rawCan stays incomplete on resync.
                if !self.dirty_reset_sent {
                    self.dirty_reset_sent = true;
                    return Some(DIRTY_RESET_SET_CAN.to_vec());
                }
                // Phase 2: send dump once, then stay silent until getCAN (or NACK→resend).
                // The reset is best-effort (some CBs return nothing for it); the
                // flush is what actually pulls the register set. Never gate the
                // flush on the reset getCAN — a silent/empty reset response must
                // not livelock the dump (live CB showed exactly this hang).
                // Re-sending on every Ping while AOA chunk-writes the first dump
                // overlaps TX/RX and produces Malformed storms that destroy the
                // large getCAN payload MyAir5 needs for :2025.
                if !self.dump_sent || self.dump_needs_resend {
                    self.dump_sent = true;
                    self.dump_needs_resend = false;
                    return Some(DUMP_SET_CAN.to_vec());
                }
                None
            }
            State::Steady => {
                // Match aaservice UartDispatchEngine.onPing:
                // ack → queued writes → else empty setCAN. Never send empty
                // setCAN while can_in_use.
                if self.ack_armed || !self.write_queue.is_empty() {
                    return Some(self.steady_tx());
                }
                if self.can_in_use {
                    return None;
                }
                Some(self.steady_tx())
            }
        }
    }

    /// Handle a non-Ping frame payload; returns zero or more engine events.
    pub(crate) fn on_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        // Every well-formed non-Ping frame updates polarity (OEM `f4170w`):
        // CRC-valid → 1, regardless of what the state machine does with it.
        self.crc_ok = true;
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
            // ackCAN polarity mirrors the last inbound frame's CRC outcome
            // (USB parity: aaservice sends ackCAN 0 when the getCAN frame
            // failed CRC). Never reset after sending: the next inbound frame
            // overwrites polarity (OEM keeps `f4170w`; the latch gates
            // emission, so a later ack reuses the current polarity).
            if self.crc_ok {
                return ACK_CAN.to_vec();
            }
            return ACK_CAN_ZERO.to_vec();
        }
        if !self.write_queue.is_empty() {
            let records = std::mem::take(&mut self.write_queue);
            self.write_flushed = true;
            // The queued flush (and any other writes) is now on the bus: only a
            // getCAN arriving after this TX may resolve the pending reads. The
            // ack branch above must never arm reads (a pure ack TX drains
            // nothing).
            for pending in &mut self.pending_reads {
                pending.flush_sent = true;
            }
            return build_set_can(&records);
        }
        EMPTY_SET_CAN.to_vec()
    }

    fn on_negotiate_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        if is_can2_in_use(payload) {
            let detail = String::from_utf8_lossy(payload).into_owned();
            // Mirror stock: flush token is already "seeded", so move to dump on next ping.
            self.can_in_use = true;
            self.state = State::RequestDump;
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
        // Stock: getCAN byte[7]=='0' → ack then resend last setCAN (stay in dump).
        if is_get_can_nack(payload) {
            self.ack_armed = true;
            if self.dump_sent {
                self.dump_needs_resend = true;
            } else if self.reset_nacks < 2 {
                // Phase-1 (dirty reset) NACK: resend the reset, bounded.
                self.reset_nacks += 1;
                self.dirty_reset_sent = false;
            }
            return Vec::new();
        }
        match parse_get_can(payload) {
            Ok(records) => {
                for record in &records {
                    self.bank.apply(record);
                    if record.reg == RegId::new(0x06) {
                        // JZ18 handshake (aa_interop §7.3): every reg-06 announcement gets
                        // an all-zero reg-07 reply echoing unit type + id.
                        self.write_queue
                            .push(build_jz18(record.unit_type, record.unit_id));
                    }
                }
                // Early getCAN before dump TX (dirty-reset response): ack it and
                // keep RequestDump until the 08 flush dump is sent.
                if !self.dump_sent {
                    self.ack_armed = true;
                    return Vec::new();
                }
                let mut events = Vec::new();
                self.state = State::Steady;
                // Stock arms ackCAN after every successful getCAN, including the dump reply.
                // Skipping the ack leaves the CB in canInUse so later polls misbehave.
                self.ack_armed = true;
                self.can_in_use = false;
                // Snapshot records come straight from the bus: nothing is ever
                // fabricated, and reg 05 is included when the dump delivered it.
                let mut bank_records: Vec<CanRecord> = Vec::new();
                for unit_type in self.bank.unit_types() {
                    bank_records.extend(self.bank.records_for_any_unit(unit_type));
                }
                let can_records: Vec<String> =
                    bank_records.iter().map(CanRecord::to_wire).collect();
                self.maybe_queue_unit_flush_for_missing_system_status();
                events.push(EngineEvent::Snapshot {
                    bank: self.bank.clone(),
                    can_records: if can_records.is_empty() {
                        None
                    } else {
                        Some(can_records)
                    },
                });
                // Reads whose flush rode a previous setCAN resolve against the
                // dump-merged bank (the early-getCAN path above returned before
                // the dump, so reads are never resolved there).
                events.extend(self.resolve_pending_reads());
                events
            }
            Err(err) => vec![EngineEvent::ProtocolWarn(format!(
                "getCAN parse failed during dump: {err:?}"
            ))],
        }
    }

    fn on_steady_frame(&mut self, payload: &[u8]) -> Vec<EngineEvent> {
        if is_can2_in_use(payload) {
            self.can_in_use = true;
            return Vec::new();
        }
        if !is_get_can(payload) {
            return vec![EngineEvent::ProtocolWarn(format!(
                "unexpected steady frame: {}",
                String::from_utf8_lossy(payload)
            ))];
        }
        self.can_in_use = false;
        match parse_get_can(payload) {
            Ok(records) => {
                for record in &records {
                    self.bank.apply(record);
                    if record.reg == RegId::new(0x06) {
                        // JZ18 handshake (aa_interop §7.3): every reg-06 announcement gets
                        // an all-zero reg-07 reply echoing unit type + id.
                        self.write_queue
                            .push(build_jz18(record.unit_type, record.unit_id));
                    }
                }
                self.ack_armed = true;
                let mut events = vec![EngineEvent::RegistersChanged { records }];
                events.extend(self.resolve_pending_reads());
                events
            }
            Err(err) => vec![EngineEvent::ProtocolWarn(format!(
                "getCAN parse failed: {err:?}"
            ))],
        }
    }

    /// Resolve pending reads whose flush was already TX'd against the bank,
    /// emitting one [`EngineEvent::RegisterRead`] per read (found or absent)
    /// and dropping it from the pending list. Reads whose flush has not been
    /// sent yet stay pending.
    fn resolve_pending_reads(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let mut remaining = Vec::new();
        for pending in std::mem::take(&mut self.pending_reads) {
            if !pending.flush_sent {
                remaining.push(pending);
                continue;
            }
            let data = if aa_registers::is_zone_bearing(pending.reg) {
                // Zone-bearing reads address a specific bank slot; a None zone
                // is treated as absent (no slot to look up).
                pending.zone.and_then(|zone| {
                    self.bank
                        .get_zone(pending.unit_type, pending.unit_id, pending.reg, zone)
                })
            } else {
                self.bank
                    .get(pending.unit_type, pending.unit_id, pending.reg)
            };
            events.push(EngineEvent::RegisterRead {
                unit_type: pending.unit_type,
                unit_id: pending.unit_id,
                reg: pending.reg,
                zone: pending.zone,
                data,
            });
        }
        self.pending_reads = remaining;
        events
    }

    /// Primary AIRCON unit for the missing-reg-05 unit flush.
    ///
    /// The flush is AIRCON-scoped by design: it must never run against a
    /// cross-type id (an 08-only bank would otherwise get a phantom
    /// AIRCON-typed record addressed to an 08 id).
    fn aircon_primary_unit_id(&self) -> Option<UnitId> {
        if self.bank.unit_ids(UnitType::AIRCON).is_empty() {
            return None;
        }
        Some(self.bank.preferred_unit_id(UnitType::AIRCON, None))
    }

    /// If the zero-uid flush dump omitted reg 05, queue one unit-scoped flush.
    fn maybe_queue_unit_flush_for_missing_system_status(&mut self) {
        let Some(unit) = self.aircon_primary_unit_id() else {
            return;
        };
        if self
            .bank
            .get(UnitType::AIRCON, unit, RegId::new(0x05))
            .is_some()
        {
            return;
        }
        self.write_queue.push(CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: unit,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Regression coverage for bring-up session fixes. When fixing a new CB
    //! session bug, add a focused test here (or in runner/feeder) that fails
    //! without the fix.

    use super::*;
    use aa_registers::{Dest, RegId, UnitId, UnitType};

    fn sample_record() -> CanRecord {
        // Reg 05 delivered by the bus for the live unit (same id that
        // announces reg 06), so no unit-scoped flush gets queued after the dump.
        CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        }
    }

    fn live_unit_reg06() -> CanRecord {
        CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
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

    /// Stock order: getSystemData → CAN2 → dirty reset → reset getCAN → ack →
    /// dump (flush seeded like aaservice open: reg-06 reset first, then 08 flush).
    fn advance_to_dump(s: &mut Session) {
        assert_eq!(s.on_ping().expect("negotiate"), GET_SYSTEM_DATA);
        let ev = s.on_frame(b"CAN2 in use");
        assert!(matches!(ev.as_slice(), [EngineEvent::Negotiated { .. }]));
        assert_eq!(s.state, State::RequestDump);
        assert!(s.can_in_use);
        // Two-phase dump: reg-06 dirty reset → its getCAN → ack → 08 flush.
        assert_eq!(s.on_ping().expect("dirty reset"), DIRTY_RESET_SET_CAN);
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&live_unit_reg06())));
        assert!(ev.is_empty(), "reset response must not emit events");
        assert_eq!(s.on_ping().expect("ack reset"), ACK_CAN);
        assert!(!s.dump_sent);
    }

    fn advance_to_steady(s: &mut Session, dump_records: &[CanRecord]) {
        advance_to_dump(s);
        assert_eq!(s.on_ping().expect("dump"), DUMP_SET_CAN);
        let ev = s.on_frame(&get_can_payload(dump_records));
        assert!(
            ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })),
            "expected Snapshot, got {ev:?}"
        );
        assert_eq!(s.state, State::Steady);
        assert!(!s.can_in_use);
        assert!(s.ack_armed);
        assert_eq!(s.on_ping().expect("ack after dump"), ACK_CAN);
    }

    #[test]
    fn expect_can2_retries_get_system_data_until_can2() {
        // Regression: silent ExpectCan2 after one getSystemData hung with no CAN2
        // then EIO. Stock re-polls getSystemData every ping until CAN2; dump only
        // after the CAN2 frame (never auto-dump on ExpectCan2 ping).
        let mut s = Session::new();
        assert_eq!(s.on_ping().unwrap(), GET_SYSTEM_DATA);
        assert_eq!(s.on_ping().unwrap(), GET_SYSTEM_DATA);
        assert_eq!(s.state, State::ExpectCan2);
        assert_eq!(s.on_ping().unwrap(), GET_SYSTEM_DATA);
        let ev = s.on_frame(b"CAN2 in use");
        assert!(matches!(ev.as_slice(), [EngineEvent::Negotiated { .. }]));
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(s.on_ping().unwrap(), DIRTY_RESET_SET_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        assert!(s.on_ping().is_none(), "must not dump twice before getCAN");
    }

    #[test]
    fn can2_then_dump_like_stock_seeded_flush() {
        // Regression: negotiation only via CAN2 in use (live CB never sends XML).
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().expect("dump"), DUMP_SET_CAN);

        let dump_rec = live_unit_reg06();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&dump_rec)));
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })));
        assert_eq!(s.state, State::Steady);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(
            s.bank
                .get(dump_rec.unit_type, dump_rec.unit_id, dump_rec.reg),
            Some(dump_rec.data)
        );
    }

    #[test]
    fn dump_get_can_arms_ack_then_empty_set_can_poll() {
        // With reg 05 already in the dump, steady uses empty setCAN (aa_interop sync).
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        // Reset-response getCAN announced reg 06, so the first steady TX is JZ18.
        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_can_records_include_real_system_status() {
        // D-9: a dump carrying a real reg 05 keeps it in the Snapshot
        // can_records (nothing is fabricated or filtered out).
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        let Some(EngineEvent::Snapshot {
            can_records: Some(recs),
            ..
        }) = ev
            .into_iter()
            .find(|e| matches!(e, EngineEvent::Snapshot { .. }))
        else {
            panic!("expected Snapshot with can_records");
        };
        assert!(
            recs.iter().any(|r| &r[9..11] == "05"),
            "can_records must include the real reg 05: {recs:?}"
        );
    }

    #[test]
    fn reg05_machinery_skipped_on_08_only_bank() {
        // F-1: the reg-05 machinery is AIRCON-scoped. On an 08-only bank no
        // AIRCON-typed record may be fabricated/queued for the 08 id (phantom
        // 07 unit, setCAN addressed as type 07 to an 08 unit). No reg 05 may
        // appear anywhere: not in the bank, not in can_records, and no
        // unit-scoped flush queued.
        let mut s = Session::new();
        s.state = State::RequestDump;
        s.dump_sent = true;
        let rf = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0ABCD).unwrap(),
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x01, 0x2c, 0x14, 0x05, 0x00],
        };
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rf)));
        let Some(EngineEvent::Snapshot {
            bank,
            can_records: Some(recs),
        }) = ev
            .into_iter()
            .find(|e| matches!(e, EngineEvent::Snapshot { .. }))
        else {
            panic!("expected Snapshot with can_records");
        };
        assert_eq!(s.state, State::Steady);
        assert!(
            bank.get(UnitType::AIRCON, rf.unit_id, RegId::new(0x05))
                .is_none(),
            "no AIRCON-typed reg 05 may be fabricated for an 08 id"
        );
        assert!(
            !bank.has_unit(UnitType::AIRCON, rf.unit_id),
            "no phantom AIRCON unit may appear"
        );
        assert!(
            recs.iter().all(|r| &r[9..11] != "05"),
            "can_records must not include reg 05 on an 08-only bank: {recs:?}"
        );
        assert!(
            s.write_queue.is_empty(),
            "no AIRCON-typed setCAN may be queued for an 08 id"
        );
        // Steady stays quiet: ack, then empty polls only.
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn snapshot_can_records_include_non_aircon_unit_records() {
        // D-3: rawCan must surface the 08 flush-dump records, not just AIRCON.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let aircon = live_unit_reg06();
        let rf = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Unknown(0x03),
            unit_id: UnitId::try_new(0x0_0F0F0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let ev = s.on_frame(&get_can_payload(&[aircon, rf]));
        let Some(EngineEvent::Snapshot {
            can_records: Some(recs),
            ..
        }) = ev
            .into_iter()
            .find(|e| matches!(e, EngineEvent::Snapshot { .. }))
        else {
            panic!("expected Snapshot with can_records");
        };
        assert!(
            recs.iter().any(|r| &r[0..2] == "08"),
            "can_records must include the 08 record: {recs:?}"
        );
        assert!(recs.iter().any(|r| &r[0..2] == "07"));
    }

    #[test]
    fn dump_without_reg05_fabricates_nothing_and_queues_unit_flush() {
        // D-9: a dump without reg 05 (AIRCON unit + zones present) yields a bank
        // with NO reg 05 slot and a Snapshot without a fabricated system_status;
        // instead a unit-scoped reg-06 flush is queued to pull the missing reg 05
        // from the bus.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let zone = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Unknown(0x03),
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x01, 0x2c, 0x14, 0x05, 0x00],
        };
        let live = live_unit_reg06();
        let ev = s.on_frame(&get_can_payload(&[live.clone(), zone]));
        let Some(EngineEvent::Snapshot {
            bank,
            can_records: Some(recs),
        }) = ev
            .into_iter()
            .find(|e| matches!(e, EngineEvent::Snapshot { .. }))
        else {
            panic!("expected Snapshot with can_records");
        };
        assert!(
            bank.get(UnitType::AIRCON, live.unit_id, RegId::new(0x05))
                .is_none(),
            "no reg 05 may be fabricated when the dump omitted it"
        );
        assert!(
            recs.iter().all(|r| &r[9..11] != "05"),
            "can_records must not fabricate reg 05: {recs:?}"
        );
        assert!(recs.iter().any(|r| &r[9..11] == "03"));
        assert!(recs.iter().any(|r| &r[9..11] == "06"));
        // Ack first, then the queued TX rides the JZ18 replies + the unit-scoped
        // reg-06 flush (two reg-06 announcements → two JZ18s).
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        let jz18 = build_jz18(UnitType::AIRCON, live.unit_id);
        let flush = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: live.unit_id,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(&[jz18.clone(), jz18, flush])
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_sent_once_until_get_can_or_nack_resend() {
        // Regression: re-TX dump on every Ping while AOA chunk-writes overlaps the
        // CB's large getCAN and yields Malformed storms.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        assert!(s.dump_sent);
        assert!(s.on_ping().is_none());
        assert!(s.on_ping().is_none());
        // NACK → ack → single resend → silent again.
        let _ = s.on_frame(b"getCAN 0");
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        assert!(s.on_ping().is_none());
    }

    #[test]
    fn dump_get_can_nack_acks_then_resends_dump() {
        // Regression: live CB often replies `getCAN 0` once before the real dump.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let ev = s.on_frame(b"getCAN 0");
        assert!(ev.is_empty());
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        assert!(s.on_ping().is_none());
    }

    #[test]
    fn dump_with_reg05_skips_unit_scoped_flush() {
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn can2_in_use_skips_empty_set_can_in_steady() {
        // Regression: empty setCAN while canInUse starves getSystemData on stock CB.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);

        let _ = s.on_frame(b"CAN2 in use");
        assert!(s.can_in_use);
        assert!(
            s.on_ping().is_none(),
            "must not send empty setCAN while can_in_use"
        );
    }

    #[test]
    fn negotiate_mismatch_warns_and_stays_alive() {
        let mut s = Session::new();
        let _ = s.on_ping();
        let ev = s.on_frame(b"CAN1 weird");
        assert!(matches!(ev.as_slice(), [EngineEvent::ProtocolWarn(_)]));
        assert!(matches!(s.state, State::Negotiate | State::ExpectCan2));
        assert_eq!(s.on_ping().unwrap(), GET_SYSTEM_DATA);
        assert_eq!(s.state, State::ExpectCan2);
        let ev = s.on_frame(b"status: CAN2 in use ok");
        assert!(matches!(ev.as_slice(), [EngineEvent::Negotiated { .. }]));
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(s.on_ping().unwrap(), DIRTY_RESET_SET_CAN);
    }

    #[test]
    fn steady_priority_ack_then_queue_then_empty() {
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);

        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);

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

        let _ = s.on_frame(&get_can_payload(&[sample_record()]));
        s.apply_cmd(EngineCmd::WriteRegisters(vec![rec]));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        let tx = s.on_ping().unwrap();
        assert!(tx.starts_with(b"setCAN "));
        assert_ne!(tx, EMPTY_SET_CAN);
        assert_ne!(tx, ACK_CAN);
    }

    #[test]
    fn write_get_can_arms_ack() {
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);

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
        advance_to_steady(&mut s, &[sample_record()]);

        s.apply_cmd(EngineCmd::ResyncMailbox);
        assert_eq!(s.state, State::RequestDump);
        assert!(!s.dirty_reset_sent);

        assert_eq!(s.on_ping().unwrap(), DIRTY_RESET_SET_CAN);
        let _ = s.on_frame(&get_can_payload(std::slice::from_ref(&live_unit_reg06())));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(ev.as_slice(), [EngineEvent::Snapshot { .. }]));
        assert_eq!(s.state, State::Steady);
    }

    #[test]
    fn early_get_can_before_dump_tx_applies_but_stays() {
        // A getCAN before the 08-flush dump TX (reset response or unsolicited) is
        // applied to the bank, acked, and RequestDump persists until the dump.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert!(!s.dump_sent);

        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(ev.is_empty());
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(
            s.bank.get(rec.unit_type, rec.unit_id, rec.reg),
            Some(rec.data)
        );

        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let ev = s.on_frame(b"getCAN 1");
        assert!(matches!(ev.as_slice(), [EngineEvent::Snapshot { .. }]));
        assert_eq!(s.state, State::Steady);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
    }

    #[test]
    fn shutdown_flag() {
        let mut s = Session::new();
        assert!(!s.is_shutdown());
        s.apply_cmd(EngineCmd::Shutdown);
        assert!(s.is_shutdown());
    }

    #[test]
    fn reg06_announcement_queues_jz18() {
        // aa_interop §7.3: every reg-06 announcement gets an all-zero reg-07
        // reply on the next TX after ack, echoing unit type + id. The dump
        // feeds reg-06 twice (reset response + dump getCAN), so two replies
        // ride the first steady TX, alongside the unit-scoped reg-06 flush
        // queued because the dump carried no reg 05.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[live_unit_reg06()]);
        let live = live_unit_reg06();
        let jz18 = build_jz18(UnitType::AIRCON, live.unit_id);
        let flush = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: live.unit_id,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let expected = build_set_can(&[jz18.clone(), jz18, flush]);
        assert_eq!(s.on_ping().unwrap(), expected);
    }

    #[test]
    fn type08_announcement_queues_type08_jz18() {
        // §7.4: split-system (type 08) announcements get a type-08 reply.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let rf = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0F0F0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rf)));
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        // Reset-response getCAN already queued the AIRCON (07) reply; the dump
        // adds the type-08 reply echoing the announcement's unit type. The
        // reg-05-less dump also queues a unit-scoped flush for the AIRCON unit.
        let flush = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: live_unit_reg06().unit_id,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let expected = build_set_can(&[
            build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id),
            build_jz18(rf.unit_type, rf.unit_id),
            flush,
        ]);
        assert_eq!(s.on_ping().unwrap(), expected);
    }

    #[test]
    fn steady_reg06_announcement_queues_jz18() {
        // Steady-state deltas also announce: reg-06 getCAN → ack then JZ18.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
        let rec = live_unit_reg06();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        let expected = build_set_can(&[build_jz18(rec.unit_type, rec.unit_id)]);
        assert_eq!(s.on_ping().unwrap(), expected);
    }

    #[test]
    fn outbound_reg06_flush_does_not_self_trigger_jz18() {
        // The daemon's own reg-06 flush writes never come back as inbound
        // announcements; only inbound getCAN reg-06 records queue JZ18.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        let jz18 = build_jz18(UnitType::AIRCON, live_unit_reg06().unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&jz18))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
        let flush = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: UnitId::try_new(0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        s.apply_cmd(EngineCmd::WriteRegisters(vec![flush.clone()]));
        let tx = s.on_ping().unwrap();
        assert_eq!(tx, build_set_can(std::slice::from_ref(&flush)));
        // No phantom JZ18 for the outbound write: next is empty poll.
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    /// Drain steady-state JZ18/empty polls until the write queue is empty.
    fn drain_steady_polls(s: &mut Session) {
        for _ in 0..4 {
            if s.write_queue.is_empty() && !s.ack_armed {
                break;
            }
            s.on_ping();
        }
        assert!(
            s.write_queue.is_empty() && !s.ack_armed,
            "steady polls must settle with an empty queue"
        );
    }

    fn read_flush(unit_type: UnitType, unit_id: UnitId) -> CanRecord {
        CanRecord {
            unit_type,
            dest: Dest::ControlBox,
            unit_id,
            reg: RegId::new(0x06),
            data: [0; 7],
        }
    }

    #[test]
    fn read_register_queues_flush_and_dedupes_identical_read() {
        // D-5: a ReadRegister queues exactly one unit-scoped reg-06 flush
        // addressed to the target unit; an identical second read dedupes the
        // flush (one bus round-trip serves both pending reads).
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        drain_steady_polls(&mut s);

        let target = live_unit_reg06();
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: target.unit_type,
            unit_id: target.unit_id,
            reg: RegId::new(0x05),
            zone: None,
        });
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: target.unit_type,
            unit_id: target.unit_id,
            reg: RegId::new(0x05),
            zone: None,
        });
        let expected_flush = read_flush(target.unit_type, target.unit_id);
        assert_eq!(s.write_queue.len(), 1, "identical reads must share a flush");
        let queued = s.write_queue[0].clone();
        assert_eq!(queued.unit_type, expected_flush.unit_type);
        assert_eq!(queued.dest, Dest::ControlBox);
        assert_eq!(queued.unit_id, expected_flush.unit_id);
        assert_eq!(queued.reg, RegId::new(0x06));
        assert_eq!(queued.data, [0; 7]);
        assert_eq!(s.pending_reads.len(), 2);
        assert!(
            s.pending_reads.iter().all(|p| !p.flush_sent),
            "reads start pending until their flush is TX'd"
        );
    }

    #[test]
    fn read_resolves_after_flush_tx_get_can() {
        // D-5: flush TX on the next ping, then a getCAN carrying the register
        // resolves the read with the fresh bank value and removes it.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        drain_steady_polls(&mut s);

        let target = live_unit_reg06();
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: target.unit_type,
            unit_id: target.unit_id,
            reg: RegId::new(0x05),
            zone: None,
        });
        let flush = read_flush(target.unit_type, target.unit_id);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&flush))
        );
        assert!(s.pending_reads[0].flush_sent);

        let fresh = CanRecord {
            unit_type: target.unit_type,
            dest: Dest::Tablet,
            unit_id: target.unit_id,
            reg: RegId::new(0x05),
            data: [0x02, 0x02, 0x04, 0x31, 0x00, 0x02, 0x00],
        };
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&fresh)));
        let read = ev
            .iter()
            .find(|e| matches!(e, EngineEvent::RegisterRead { .. }))
            .expect("register read must resolve after flush TX");
        match read {
            EngineEvent::RegisterRead {
                unit_type,
                unit_id,
                reg,
                zone,
                data: Some(data),
            } => {
                assert_eq!(*unit_type, fresh.unit_type);
                assert_eq!(*unit_id, fresh.unit_id);
                assert_eq!(*reg, fresh.reg);
                assert_eq!(*zone, None);
                assert_eq!(*data, fresh.data);
            }
            other => panic!("expected resolved read, got {other:?}"),
        }
        assert!(
            s.pending_reads.is_empty(),
            "resolved reads must be removed from the pending list"
        );
    }

    #[test]
    fn read_absent_register_emits_none() {
        // D-5: a register the flush getCAN never delivers resolves to None
        // (never a stale-bank guess), and the read is still consumed.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        drain_steady_polls(&mut s);

        let target = live_unit_reg06();
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: target.unit_type,
            unit_id: target.unit_id,
            reg: RegId::new(0x02),
            zone: None,
        });
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&read_flush(
                target.unit_type,
                target.unit_id
            )))
        );
        let ev = s.on_frame(b"getCAN 1");
        let read = ev
            .iter()
            .find(|e| matches!(e, EngineEvent::RegisterRead { .. }))
            .expect("absent register must still resolve to None");
        assert!(
            matches!(read, EngineEvent::RegisterRead { data: None, .. }),
            "expected RegisterRead with None, got {read:?}"
        );
        assert!(s.pending_reads.is_empty());
    }

    #[test]
    fn pre_flush_get_can_does_not_resolve_read() {
        // D-5 flush_sent gate: a spontaneous getCAN before the flush was TX'd
        // must not answer (or error) the pending read; it resolves only after
        // the ping that transmits the flush.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        drain_steady_polls(&mut s);

        let target = live_unit_reg06();
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: target.unit_type,
            unit_id: target.unit_id,
            reg: RegId::new(0x05),
            zone: None,
        });

        let rec = sample_record();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(
            !ev.iter()
                .any(|e| matches!(e, EngineEvent::RegisterRead { .. })),
            "pre-flush getCAN must not resolve the read: {ev:?}"
        );
        assert_eq!(s.pending_reads.len(), 1);
        assert!(!s.pending_reads[0].flush_sent);

        // The pre-flush getCAN armed ackCAN; ack first, then the flush rides
        // the next ping.
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&read_flush(
                target.unit_type,
                target.unit_id
            )))
        );
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&rec)));
        assert!(matches!(
            ev.iter()
                .find(|e| matches!(e, EngineEvent::RegisterRead { .. })),
            Some(EngineEvent::RegisterRead { data: Some(_), .. })
        ));
        assert!(s.pending_reads.is_empty());
    }

    #[test]
    fn zone_read_resolves_addressed_zone_slot() {
        // D-5: zone-bearing reads resolve the addressed zone's bank slot
        // (bank.get_zone), so different zones resolve independently — a zone
        // never delivered stays None while the delivered one returns fresh
        // data from its own slot.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let z1 = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Unknown(0x03),
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x01, 0x2c, 0x14, 0x05, 0x00],
        };
        let z2 = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Unknown(0x03),
            unit_id: z1.unit_id,
            reg: RegId::new(0x03),
            data: [0x02, 0xe4, 0x02, 0x2d, 0x15, 0x06, 0x01],
        };
        let ev = s.on_frame(&get_can_payload(&[z1.clone(), z2]));
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })));
        assert_eq!(s.state, State::Steady);
        // Dump omitted reg 05 → ack, then the JZ18 + unit flush ride out.
        drain_steady_polls(&mut s);
        assert!(s.write_queue.is_empty());

        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: z1.unit_type,
            unit_id: z1.unit_id,
            reg: z1.reg,
            zone: Some(0x01),
        });
        s.apply_cmd(EngineCmd::ReadRegister {
            unit_type: z1.unit_type,
            unit_id: z1.unit_id,
            reg: z1.reg,
            zone: Some(0x03),
        });
        // Both reads share one flush (identical target + reg 06 + zero data).
        assert_eq!(s.write_queue.len(), 1);
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&read_flush(z1.unit_type, z1.unit_id)))
        );

        // Fresh zone-1 payload on the flush getCAN; zone 3 is never delivered.
        let mut fresh_z1 = z1;
        fresh_z1.data = [0x01, 0xff, 0x03, 0x40, 0x20, 0x0a, 0x02];
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&fresh_z1)));
        let reads: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                EngineEvent::RegisterRead {
                    unit_type: _,
                    unit_id: _,
                    reg: _,
                    zone,
                    data,
                } => Some((*zone, *data)),
                _ => None,
            })
            .collect();
        assert_eq!(reads.len(), 2, "both zone reads resolve: {reads:?}");
        assert!(
            reads.contains(&(Some(0x01), Some(fresh_z1.data))),
            "zone 1 must read its fresh slot: {reads:?}"
        );
        assert!(
            reads.contains(&(Some(0x03), None)),
            "undelivered zone 3 must resolve None: {reads:?}"
        );
        assert!(s.pending_reads.is_empty());
    }

    /// Drain steady-state JZ18/empty polls until the ack latch and queue settle.
    fn settle_steady(s: &mut Session) {
        assert_eq!(
            s.on_ping().unwrap(),
            build_set_can(std::slice::from_ref(&build_jz18(
                UnitType::AIRCON,
                live_unit_reg06().unit_id
            )))
        );
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn lone_crc_failed_getcan_arms_ack_can_zero() {
        // OEM f4169v is CRC-independent: a CRC-failed getCAN arms the latch, so
        // the next ping carries ackCAN 0 with no good frame in between; the
        // latch is cleared by emission and must not repeat on the next ping.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        s.on_crc_failure(true);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN_ZERO);
        assert_eq!(
            s.on_ping().unwrap(),
            EMPTY_SET_CAN,
            "latch cleared: no second ackCAN without a new frame"
        );
    }

    #[test]
    fn bad_then_good_getcan_no_polarity_leak() {
        // [bad, good] getCAN → ackCAN 1: the good frame overwrites polarity.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        s.on_crc_failure(true);
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&sample_record())));
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
    }

    #[test]
    fn good_then_bad_getcan_ack_zero() {
        // [good, bad] getCAN → ackCAN 0: last complete frame's outcome wins.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&sample_record())));
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        s.on_crc_failure(true);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN_ZERO);
    }

    #[test]
    fn bad_non_getcan_flips_armed_ack_to_zero() {
        // Every complete non-Ping frame updates polarity: a CRC-failed
        // non-getCAN drops an ack armed by an earlier good getCAN to 0, while
        // the latch (armed by the getCAN) still gates emission.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&sample_record())));
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        s.on_crc_failure(false);
        assert_eq!(s.on_ping().unwrap(), ACK_CAN_ZERO);
    }

    #[test]
    fn bad_non_getcan_alone_does_not_arm() {
        // Arming is getCAN-specific: a CRC-failed non-getCAN drops polarity to
        // 0 but must not produce an ackCAN at all (no latch → normal empty poll).
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        s.on_crc_failure(false);
        assert_eq!(
            s.on_ping().unwrap(),
            EMPTY_SET_CAN,
            "no latch: a CRC-failed non-getCAN must not emit ackCAN"
        );
    }

    #[test]
    fn good_non_getcan_after_bad_restores_polarity() {
        // A well-formed non-getCAN frame (ProtocolWarn path) still overwrites
        // polarity: an ack armed by a failed getCAN goes out as ackCAN 1.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        settle_steady(&mut s);

        s.on_crc_failure(true);
        let ev = s.on_frame(b"bogus steady frame");
        assert!(
            matches!(ev.as_slice(), [EngineEvent::ProtocolWarn(_)]),
            "non-getCAN in steady must warn: {ev:?}"
        );
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
    }
}
