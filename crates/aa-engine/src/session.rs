//! Sync CB session state machine (no I/O).

use aa_registers::{
    CanRecord, DecodedRegister, Dest, Fan, FreshAir, Mode, Power, RegId, RegisterBank,
    SystemStatus, UnitId, UnitType,
};

use crate::event::{EngineCmd, EngineEvent};
use crate::wire::{
    ACK_CAN, ACK_CAN_ZERO, DIRTY_RESET_SET_CAN, DUMP_SET_CAN, EMPTY_SET_CAN, GET_SYSTEM_DATA,
    build_set_can, is_can2_in_use, is_get_can, is_get_can_nack, parse_get_can,
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
    /// True once the bus delivered a real reg 05 (system status) in a getCAN.
    /// The snapshot rawCan includes reg 05 only then; a bank slot synthesized
    /// from zones/XML must stay out of `MyAir5` rawCan (USB parity).
    system_status_real: bool,
    /// Raw direct-message queue (one-shot polls / `setAllZoneSensorData` etc.).
    direct_queue: Vec<Vec<u8>>,
    /// Most recent direct payload written; the next non-getCAN frame is its reply.
    last_direct_sent: Vec<u8>,
    /// Set when `steady_tx` drained the write queue; consumed by the runner to
    /// emit [`EngineEvent::WriteFlushed`] after the frame is transmitted.
    write_flushed: bool,
    /// Last inbound frame CRC outcome; feeds the outbound `ackCAN 0|1` polarity.
    crc_ok: bool,
    /// System status parsed from getSystemData XML before the dump reveals unit id.
    pending_system: Option<SystemStatus>,
    /// Mirrors stock `canInUse`: after `CAN2 in use`, skip empty `setCAN`.
    can_in_use: bool,
    /// Ping counter while hunting getSystemData XML (aaservice poll path).
    system_xml_ticks: u32,
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
            dump_needs_resend: false,
            dirty_reset_sent: false,
            reset_nacks: 0,
            system_status_real: false,
            direct_queue: Vec::new(),
            last_direct_sent: Vec::new(),
            write_flushed: false,
            crc_ok: true,
            pending_system: None,
            can_in_use: false,
            system_xml_ticks: 0,
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

    /// Record the last inbound frame's CRC outcome (drives `ackCAN 0|1`).
    pub(crate) const fn set_crc_ok(&mut self, ok: bool) {
        self.crc_ok = ok;
    }

    /// Apply an inbound engine command.
    pub(crate) fn apply_cmd(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::WriteRegisters(records) => {
                self.write_queue.extend(records);
            }
            EngineCmd::WriteDirect(payload) => {
                if !self.direct_queue.contains(&payload) {
                    self.direct_queue.push(payload);
                }
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
                // ack → queued writes → direct queue → (while missing system
                // status) getSystemData poll → else empty setCAN. Never send
                // empty setCAN while can_in_use, and never send empty setCAN
                // while still hunting getSystemData XML — aaservice skips empty
                // setCAN because it leaves the CB returning "CAN2 in use" for
                // getSystemData forever (starves :2025 / MyAir5).
                if self.ack_armed || !self.write_queue.is_empty() {
                    return Some(self.steady_tx());
                }
                if !self.direct_queue.is_empty() {
                    self.last_direct_sent = self.direct_queue.remove(0);
                    self.write_flushed = true;
                    return Some(self.last_direct_sent.clone());
                }
                if self.needs_system_status_poll() {
                    self.system_xml_ticks = self.system_xml_ticks.saturating_add(1);
                    return Some(GET_SYSTEM_DATA.to_vec());
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
            // ackCAN polarity mirrors the last inbound CRC outcome (USB parity:
            // aaservice sends ackCAN 0 when the getCAN frame failed CRC).
            if self.crc_ok {
                return ACK_CAN.to_vec();
            }
            self.crc_ok = true;
            return ACK_CAN_ZERO.to_vec();
        }
        if !self.write_queue.is_empty() {
            let records = std::mem::take(&mut self.write_queue);
            self.write_flushed = true;
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
        } else if let Some(status) = parse_system_data_xml_status(payload) {
            // Rare: XML before dump (bus already free). Hold until dump reveals unit id.
            self.can_in_use = false;
            self.pending_system = Some(status);
            self.state = State::RequestDump;
            vec![EngineEvent::Negotiated {
                detail: "getSystemData xml (pending apply after dump)".into(),
            }]
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
                    if record.reg == RegId::new(0x05) {
                        self.system_status_real = true;
                    }
                }
                // Early getCAN before dump TX (dirty-reset response): ack it and
                // keep RequestDump until the 08 flush dump is sent.
                if !self.dump_sent {
                    self.ack_armed = true;
                    return Vec::new();
                }
                let mut events = Vec::new();
                // Reg-05 synthesis is AIRCON-scoped: skip on banks without an
                // AIRCON unit rather than applying a 07-typed record to the
                // cross-type fallback id (phantom unit on 08-only banks).
                if let Some(status) = self.pending_system.take()
                    && let Some(unit) = self.aircon_primary_unit_id()
                {
                    let record = system_status_record(unit, status);
                    self.bank.apply(&record);
                    events.push(EngineEvent::RegistersChanged {
                        records: vec![record],
                    });
                }
                self.state = State::Steady;
                // Stock arms ackCAN after every successful getCAN, including the dump reply.
                // Skipping the ack leaves the CB in canInUse so later polls misbehave.
                self.ack_armed = true;
                self.can_in_use = false;
                // Capture CB dump hex *before* synthesizing reg 05 into the bank:
                // full bank across all unit types (dirty-reset + 08-flush dumps
                // and any steady-state deltas), excluding a synthesized reg 05
                // that never came from the bus (synthesis is AIRCON-primary only,
                // so retaining only real reg-05s stays safe). MyAir5 rawCan must
                // match USB; typed system_status still comes from the bank slot
                // via mailbox DTOs.
                let mut bank_records: Vec<CanRecord> = Vec::new();
                for unit_type in self.bank.unit_types() {
                    bank_records.extend(self.bank.records_for_any_unit(unit_type));
                }
                if !self.system_status_real {
                    bank_records.retain(|r| r.reg != RegId::new(0x05));
                }
                let can_records: Vec<String> =
                    bank_records.iter().map(CanRecord::to_wire).collect();
                self.maybe_synthesize_system_status_from_zones();
                self.maybe_queue_unit_flush_for_missing_system_status();
                events.push(EngineEvent::Snapshot {
                    bank: self.bank.clone(),
                    can_records: if can_records.is_empty() {
                        None
                    } else {
                        Some(can_records)
                    },
                });
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
        if let Some(status) = parse_system_data_xml_status(payload) {
            self.can_in_use = false;
            // Reg-05 synthesis is AIRCON-scoped: skip on banks without an
            // AIRCON unit rather than applying a 07-typed record to the
            // cross-type fallback id (phantom unit on 08-only banks).
            let Some(unit) = self.aircon_primary_unit_id() else {
                return Vec::new();
            };
            let record = system_status_record(unit, status);
            self.bank.apply(&record);
            return vec![EngineEvent::RegistersChanged {
                records: vec![record],
            }];
        }
        if !is_get_can(payload) {
            // A non-getCAN frame while a direct request is outstanding is its
            // reply (e.g. XML for setAllZoneSensorData / a one-shot poll tag).
            if !self.last_direct_sent.is_empty() {
                self.last_direct_sent.clear();
                return vec![EngineEvent::DirectReply {
                    payload: payload.to_vec(),
                }];
            }
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
                    if record.reg == RegId::new(0x05) {
                        self.system_status_real = true;
                    }
                }
                self.ack_armed = true;
                vec![EngineEvent::RegistersChanged { records }]
            }
            Err(err) => vec![EngineEvent::ProtocolWarn(format!(
                "getCAN parse failed: {err:?}"
            ))],
        }
    }

    /// Primary AIRCON unit for reg-05 synthesis/poll machinery.
    ///
    /// Reg-05 synthesis and getSystemData polling are AIRCON-scoped by design:
    /// they must never run against a cross-type id (an 08-only bank would
    /// otherwise get a phantom AIRCON-typed record addressed to an 08 id).
    fn aircon_primary_unit_id(&self) -> Option<UnitId> {
        if self.bank.unit_ids(UnitType::AIRCON).is_empty() {
            return None;
        }
        Some(self.bank.preferred_unit_id(UnitType::AIRCON, None))
    }

    fn needs_system_status_poll(&self) -> bool {
        if self.pending_system.is_some() {
            return false;
        }
        let Some(unit) = self.aircon_primary_unit_id() else {
            return false;
        };
        self.bank
            .get(UnitType::AIRCON, unit, RegId::new(0x05))
            .is_none()
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

    /// Seed reg 05 from zone setpoints when the dump omitted it.
    ///
    /// Without this, mailbox snapshots have zones but no `system_status`, so aaservice
    /// never emits `getSystemData` and `MyAir5` `:2025` keeps `aircons: {}`.
    fn maybe_synthesize_system_status_from_zones(&mut self) {
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
        // Fallback when the dump never delivered reg 05: synthesize from zones,
        // choosing the zone with the largest |measured − set| differential (the
        // room that most needs conditioning — MyTemp semantics). Hardcoding
        // zone 1 here told MyAir5 "myzone=1" on every dump lacking reg 05,
        // which can suppress its own auto-move decision.
        let mut set_temp_x2 = 48; // 24°C fallback
        let mut myzone_id = 1u8;
        let mut max_diff_x2 = -1.0f64;
        for z in 1u8..=10 {
            if let Some(DecodedRegister::ZoneState(state)) =
                self.bank
                    .get_zone_decoded(UnitType::AIRCON, unit, RegId::new(0x03), z)
            {
                let meas_x2 = (f64::from(state.meas_int) + f64::from(state.meas_dec) / 10.0) * 2.0;
                let diff = (meas_x2 - f64::from(state.set_temp_x2)).abs();
                if diff > max_diff_x2 {
                    max_diff_x2 = diff;
                    set_temp_x2 = state.set_temp_x2;
                    myzone_id = state.zone;
                }
            }
        }
        let status = SystemStatus {
            power: Power::On,
            mode: Mode::Cool,
            fan: Fan::Auto,
            set_temp_x2,
            myzone_id,
            fresh_air: FreshAir::None,
            rf_sys_id: 0,
        };
        self.bank.apply(&system_status_record(unit, status));
    }
}

/// Pull `<tag>…</tag>` inner text from a CB XML-ish payload.
fn xml_tag<'a>(payload: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = payload.find(&open)? + open.len();
    let end = payload[start..].find(&close)? + start;
    Some(payload[start..end].trim())
}

fn system_status_record(unit_id: UnitId, status: SystemStatus) -> CanRecord {
    CanRecord {
        unit_type: UnitType::AIRCON,
        dest: Dest::Tablet,
        unit_id,
        reg: RegId::new(0x05),
        data: status.into(),
    }
}

/// Map stock `getSystemData` XML into a [`SystemStatus`].
fn parse_system_data_xml_status(payload: &[u8]) -> Option<SystemStatus> {
    let text = std::str::from_utf8(payload).ok()?;
    if !text.contains("<request>getSystemData</request>") {
        return None;
    }
    let power_s = xml_tag(text, "state")?.to_ascii_lowercase();
    let power = match power_s.as_str() {
        "on" => Power::On,
        "off" => Power::Off,
        _ => return None,
    };
    let mode_s = xml_tag(text, "mode")?.to_ascii_lowercase();
    let mode = match mode_s.as_str() {
        "cool" => Mode::Cool,
        "heat" => Mode::Heat,
        "vent" => Mode::Vent,
        "auto" => Mode::Auto,
        "dry" => Mode::Dry,
        "myauto" => Mode::MyAuto,
        _ => return None,
    };
    let fan_s = xml_tag(text, "fan")?.to_ascii_lowercase();
    let fan = match fan_s.as_str() {
        "off" => Fan::Off,
        "low" => Fan::Low,
        "medium" | "med" => Fan::Medium,
        "high" => Fan::High,
        "auto" => Fan::Auto,
        "autoaa" => Fan::AutoAa,
        _ => return None,
    };
    let set_temp_c: f32 = xml_tag(text, "setTemp")?.parse().ok()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let set_temp_x2 = (set_temp_c * 2.0).round().clamp(0.0, 255.0) as u8;
    let myzone_id: u8 = xml_tag(text, "myZone")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let fresh_s = xml_tag(text, "freshAir")
        .unwrap_or("none")
        .to_ascii_lowercase();
    let fresh_air = match fresh_s.as_str() {
        "on" => FreshAir::On,
        "off" => FreshAir::Off,
        _ => FreshAir::None,
    };
    Some(SystemStatus {
        power,
        mode,
        fan,
        set_temp_x2,
        myzone_id,
        fresh_air,
        rf_sys_id: 0,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Regression coverage for bring-up session fixes. When fixing a new CB
    //! session bug, add a focused test here (or in runner/feeder) that fails
    //! without the fix.

    use super::*;
    use aa_registers::{Dest, RegId, UnitId, UnitType};

    /// Minimal stock-shaped getSystemData XML (tags the parser requires).
    const SAMPLE_SYSTEM_XML: &[u8] = b"<request>getSystemData</request>
<aircon><info><state>on</state><mode>cool</mode><fan>high</fan>
<setTemp>24.0</setTemp><myZone>1</myZone><freshAir>none</freshAir></info></aircon>";

    fn sample_record() -> CanRecord {
        CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
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
        // Regression: waiting for XML before dump loops forever (live CB only returns CAN2).
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
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_can_records_exclude_synthesized_system_status() {
        // Regression: USB rawCan has no reg 05; synthesized 05 must stay in the
        // typed bank/DTO only, not in snapshot can_records for MyAir5.
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
                .is_some(),
            "bank still has synthesized reg 05"
        );
        assert!(
            recs.iter().all(|r| &r[9..11] != "05"),
            "can_records must not include synthesized 05: {recs:?}"
        );
        assert!(recs.iter().any(|r| &r[9..11] == "03"));
        assert!(recs.iter().any(|r| &r[9..11] == "06"));
    }

    #[test]
    fn reg05_machinery_skipped_on_08_only_bank() {
        // F-1: the reg-05 machinery is AIRCON-scoped. On an 08-only bank no
        // AIRCON-typed record may be synthesized/queued for the 08 id (phantom
        // 07 unit, setCAN addressed as type 07 to an 08 unit).
        let mut s = Session::new();
        let rf = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0ABCD).unwrap(),
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x01, 0x2c, 0x14, 0x05, 0x00],
        };
        s.bank.apply(&rf);

        s.maybe_synthesize_system_status_from_zones();
        assert!(
            s.bank
                .get(UnitType::AIRCON, rf.unit_id, RegId::new(0x05))
                .is_none(),
            "no AIRCON-typed reg 05 may be synthesized for an 08 id"
        );
        assert!(
            !s.bank.has_unit(UnitType::AIRCON, rf.unit_id),
            "no phantom AIRCON unit may appear"
        );
        assert!(
            !s.needs_system_status_poll(),
            "no getSystemData poll on an 08-only bank"
        );
        s.maybe_queue_unit_flush_for_missing_system_status();
        assert!(
            s.write_queue.is_empty(),
            "no AIRCON-typed setCAN may be queued for an 08 id"
        );
    }

    #[test]
    fn steady_system_xml_skipped_on_08_only_bank() {
        // F-1: steady-state getSystemData XML must not synthesize an
        // AIRCON-typed reg 05 for the cross-type fallback id on an 08-only
        // bank (phantom 07 unit).
        let mut s = Session::new();
        s.state = State::Steady;
        let rf = CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_0ABCD).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        s.bank.apply(&rf);
        let ev = s.on_frame(SAMPLE_SYSTEM_XML);
        assert!(ev.is_empty(), "no reg-05 event on an 08-only bank: {ev:?}");
        assert!(
            s.bank
                .get(UnitType::AIRCON, rf.unit_id, RegId::new(0x05))
                .is_none(),
            "no AIRCON-typed reg 05 may be synthesized for an 08 id"
        );
        assert!(
            s.bank.unit_ids(UnitType::AIRCON).is_empty(),
            "no phantom AIRCON unit may appear"
        );
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
    fn dump_without_reg05_synthesizes_system_status_from_zones() {
        // Regression: live dump has zones but never reg 05; UART getSystemData stays
        // CAN2. Mailbox needs system_status so MyAir5 :2025 gets a non-empty aircons.
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
        let ev = s.on_frame(&get_can_payload(&[live, zone]));
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })));
        let data = s
            .bank
            .get(
                UnitType::AIRCON,
                UnitId::try_new(0x0_11111).unwrap(),
                RegId::new(0x05),
            )
            .expect("synthesized reg 05");
        let status = SystemStatus::from(data);
        assert_eq!(status.set_temp_x2, 0x2c);
        assert_eq!(status.myzone_id, 1);
        assert_eq!(status.power, Power::On);
        // No unit flush / getSystemData hunt once system_status exists.
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_without_reg05_synthesizes_fallback_without_zones() {
        // Even with no zone records, seed a default system_status so :2025 can create
        // an aircon shell (zone events may arrive later).
        let mut s = Session::new();
        advance_to_steady(&mut s, &[live_unit_reg06()]);
        let data = s
            .bank
            .get(
                UnitType::AIRCON,
                UnitId::try_new(0x0_11111).unwrap(),
                RegId::new(0x05),
            )
            .expect("fallback reg 05");
        assert_eq!(SystemStatus::from(data).set_temp_x2, 48);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_without_reg05_skips_unit_flush_after_synthesis() {
        // With synthesis seeding reg 05, unit-scoped flush is skipped (reg present).
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let live = live_unit_reg06();
        let _ = s.on_frame(&get_can_payload(std::slice::from_ref(&live)));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
        assert!(
            s.bank
                .get(UnitType::AIRCON, live.unit_id, RegId::new(0x05))
                .is_some()
        );
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
    fn steady_applies_system_xml_if_cb_ever_sends_it() {
        // aaservice USB path: getSystemData poll eventually returns XML for MyAir5/:2025.
        // After dump we synthesize reg 05; XML still overwrites when the bus ever returns it.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[live_unit_reg06()]);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);

        let ev = s.on_frame(SAMPLE_SYSTEM_XML);
        assert!(matches!(
            ev.as_slice(),
            [EngineEvent::RegistersChanged { .. }]
        ));
        let live = UnitId::try_new(0x0_11111).unwrap();
        let data = s
            .bank
            .get(UnitType::AIRCON, live, RegId::new(0x05))
            .expect("reg 05 from XML");
        let status = SystemStatus::from(data);
        assert_eq!(status.power, Power::On);
        assert_eq!(status.mode, Mode::Cool);
        assert_eq!(status.set_temp_x2, 48);
        assert_eq!(status.fan, Fan::High);
        // Once system status exists, fall back to empty setCAN sync.
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn dump_with_reg05_skips_unit_scoped_flush() {
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);
    }

    #[test]
    fn can2_in_use_skips_empty_set_can_in_steady() {
        // Regression: empty setCAN while canInUse starves getSystemData on stock CB.
        let mut s = Session::new();
        advance_to_steady(&mut s, &[sample_record()]);
        assert_eq!(s.on_ping().unwrap(), EMPTY_SET_CAN);

        let _ = s.on_frame(b"CAN2 in use");
        assert!(s.can_in_use);
        assert!(
            s.on_ping().is_none(),
            "must not send empty setCAN while can_in_use"
        );
    }

    #[test]
    fn xml_before_dump_still_applies_to_live_unit_after_dump() {
        // Optional path: bus free enough to return XML before the flush dump.
        let mut s = Session::new();
        assert_eq!(s.on_ping().unwrap(), GET_SYSTEM_DATA);
        let _ = s.on_frame(SAMPLE_SYSTEM_XML);
        assert!(s.pending_system.is_some());
        assert_eq!(s.state, State::RequestDump);
        assert_eq!(s.on_ping().unwrap(), DIRTY_RESET_SET_CAN);
        let _ = s.on_frame(&get_can_payload(std::slice::from_ref(&live_unit_reg06())));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);

        let live = live_unit_reg06();
        let ev = s.on_frame(&get_can_payload(std::slice::from_ref(&live)));
        assert!(
            ev.iter()
                .any(|e| matches!(e, EngineEvent::RegistersChanged { .. }))
        );
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Snapshot { .. })));
        assert_eq!(s.on_ping().unwrap(), ACK_CAN);

        let data = s
            .bank
            .get(UnitType::AIRCON, live.unit_id, RegId::new(0x05))
            .expect("reg 05 applied to live unit");
        assert_eq!(SystemStatus::from(data).fan, Fan::High);
    }

    #[test]
    fn parse_system_data_xml_status_round_trip_fields() {
        let status = parse_system_data_xml_status(SAMPLE_SYSTEM_XML).expect("parse");
        assert_eq!(status.power, Power::On);
        assert_eq!(status.mode, Mode::Cool);
        assert_eq!(status.fan, Fan::High);
        assert_eq!(status.set_temp_x2, 48);
        assert_eq!(status.myzone_id, 1);
        assert_eq!(status.fresh_air, FreshAir::None);
        assert!(parse_system_data_xml_status(b"CAN2 in use").is_none());
        assert!(parse_system_data_xml_status(b"<request>other</request>").is_none());
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
    fn dump_without_reg05_synthesizes_worst_off_zone_as_myzone() {
        // Regression: synthesis hardcoded zone 1; MyAir5 was told myzone=1 on
        // every dump lacking reg 05, suppressing its auto-move. The synthesized
        // myzone must be the zone with the largest |measured - set| diff.
        let mut s = Session::new();
        advance_to_dump(&mut s);
        assert_eq!(s.on_ping().unwrap(), DUMP_SET_CAN);
        let zone1 = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Unknown(0x03),
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x01, 0x2c, 0x14, 0x05, 0x00], // zone 1, meas 20.5C -> diff 3 (small)
        };
        let zone3 = CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Unknown(0x03),
            unit_id: UnitId::try_new(0x0_11111).unwrap(),
            reg: RegId::new(0x03),
            data: [0x03, 0x64, 0x01, 0x2c, 0x14, 0x00, 0x00], // zone 3, meas 20.0C -> diff 4 (large)
        };
        let live = live_unit_reg06();
        let _ = s.on_frame(&get_can_payload(&[live, zone1, zone3]));
        let data = s
            .bank
            .get(
                UnitType::AIRCON,
                UnitId::try_new(0x0_11111).unwrap(),
                RegId::new(0x05),
            )
            .expect("synthesized reg 05");
        let status = SystemStatus::from(data);
        assert_eq!(status.myzone_id, 3);
    }
}
