//! Pure converters between [`RegisterBank`] and northbound mailbox JSON.

use std::collections::BTreeMap;

use aa_registers::{
    CanRecord, DecodedRegister, Dest, Fan, FreshAir, Mode, Power, RegId, RegisterBank, SensorType,
    SystemStatus, UnitId, UnitType, ZoneConfig, ZoneState,
};
use serde::Deserialize;
use serde_json::Value;

use crate::dto::{SnapshotBody, SystemStatusDto, ZoneConfigDto, ZoneDto};
use crate::error::EncodeError;
use crate::message::ServerMessage;

/// Zone ids scanned when building a snapshot (`1`..=`10`).
const ZONE_SCAN: std::ops::RangeInclusive<u8> = 1..=10;

/// Default zone-config header when synthesizing CB→tablet bytes from a DTO.
const ZONE_CONFIG_HEADER_CB_TO_TABLET: u8 = 0x20;

/// Build a `mailbox_snapshot` from one unit in `bank`, optionally overriding
/// `can_records` (CB dump hex for `MyAir5` `rawCan`; excludes synthesized regs).
#[must_use]
pub fn snapshot_from_bank_with_can_records(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
    can_records: Option<Vec<String>>,
) -> ServerMessage {
    let mut body = snapshot_body_from_bank(bank, unit_type, unit_id);
    if let Some(recs) = can_records {
        body.can_records = if recs.is_empty() { None } else { Some(recs) };
    }
    ServerMessage::from_snapshot_body(body)
}

/// Build a `mailbox_snapshot` [`ServerMessage`] from one unit in `bank`.
///
/// Absent registers become omitted optional fields. An empty zones map is
/// omitted (`None`), not serialized as `{}`.
#[must_use]
pub fn snapshot_from_bank(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
) -> ServerMessage {
    snapshot_from_bank_with_can_records(bank, unit_type, unit_id, None)
}

/// Snapshot body fields for one unit (without the message `type` tag).
#[must_use]
pub fn snapshot_body_from_bank(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
) -> SnapshotBody {
    let system_status = bank
        .get_decoded(unit_type, unit_id, RegId::new(0x05))
        .and_then(|decoded| match decoded {
            DecodedRegister::SystemStatus(s) => Some(system_status_to_dto(&s)),
            _ => None,
        });

    let zone_config = bank
        .get_decoded(unit_type, unit_id, RegId::new(0x01))
        .and_then(|decoded| match decoded {
            DecodedRegister::ZoneConfig(c) => Some(zone_config_to_dto(&c)),
            _ => None,
        });

    let mut zones = BTreeMap::new();
    for z in ZONE_SCAN {
        if let Some(DecodedRegister::ZoneState(state)) =
            bank.get_zone_decoded(unit_type, unit_id, RegId::new(0x03), z)
        {
            zones.insert(z.to_string(), zone_dto_from_state(&state));
        }
    }
    let zones = if zones.is_empty() { None } else { Some(zones) };

    let can_records: Vec<String> = bank
        .records_for_unit(unit_type, unit_id)
        .into_iter()
        .map(|r| r.to_wire())
        .collect();
    let can_records = if can_records.is_empty() {
        None
    } else {
        Some(can_records)
    };

    SnapshotBody {
        unit_id: unit_id.to_string(),
        system_status,
        zone_config,
        zones,
        can_records,
    }
}

/// Encode a client `mailbox_update` register + payload into ControlBox-bound
/// [`CanRecord`]s.
///
/// Supported register keys: `system_status`, `zone_state`.
/// `zone_config` and all other keys return [`EncodeError::UnsupportedRegister`].
///
/// # Errors
///
/// Returns [`EncodeError`] for unsupported registers, bad payloads, or unknown
/// enum strings.
pub fn records_from_update(
    unit_type: UnitType,
    unit_id: UnitId,
    register: &str,
    payload: &Value,
) -> Result<Vec<CanRecord>, EncodeError> {
    records_from_update_with_bank(&RegisterBank::new(), unit_type, unit_id, register, payload)
}

/// Partial `system_status` write patch: only provided fields are applied over
/// the current bank value. aaservice's `setAircon` mapper sends sparse payloads
/// (no `myzone_id` / `fresh_air`), and writing defaults for those would stomp
/// real CB state.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SystemStatusPatch {
    power: Option<String>,
    mode: Option<String>,
    fan: Option<String>,
    target_temp_c: Option<f64>,
    myzone_id: Option<u8>,
    fresh_air: Option<bool>,
}

/// Partial `zone_state` write patch (`zone_id` required).
#[derive(Debug, Clone, Deserialize)]
struct ZoneStatePatch {
    zone_id: ZoneIdJson,
    #[serde(default)]
    open: Option<bool>,
    #[serde(default)]
    damper_pct: Option<u8>,
    #[serde(default)]
    sensor_type: Option<String>,
    #[serde(default)]
    target_temp_c: Option<f64>,
    #[serde(default)]
    measured_temp_c: Option<f64>,
}

/// Encode a `mailbox_update` as ControlBox-bound records.
///
/// Partial payloads are merged over the current `bank` register values (USB
/// parity: the CB parses sparse `setAircon` JSON itself and only touches
/// provided fields).
///
/// # Errors
///
/// Returns [`EncodeError`] for unsupported registers, bad payloads, or unknown
/// enum strings.
pub fn records_from_update_with_bank(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
    register: &str,
    payload: &Value,
) -> Result<Vec<CanRecord>, EncodeError> {
    match register {
        "system_status" => encode_system_status_update(bank, unit_type, unit_id, payload),
        "zone_state" => encode_zone_state_update(bank, unit_type, unit_id, payload),
        other => Err(EncodeError::UnsupportedRegister(other.to_owned())),
    }
}

fn encode_system_status_update(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
    payload: &Value,
) -> Result<Vec<CanRecord>, EncodeError> {
    let patch: SystemStatusPatch = deserialize_payload(payload)?;
    let base = match bank.get_decoded(unit_type, unit_id, RegId::new(0x05)) {
        Some(DecodedRegister::SystemStatus(status)) => status,
        _ => SystemStatus {
            power: Power::On,
            mode: Mode::Cool,
            fan: Fan::Auto,
            set_temp_x2: 48,
            myzone_id: 1,
            fresh_air: FreshAir::None,
            rf_sys_id: 0,
        },
    };
    let status = SystemStatus {
        power: patch
            .power
            .as_deref()
            .map(power_from_str)
            .transpose()?
            .unwrap_or(base.power),
        mode: patch
            .mode
            .as_deref()
            .map(mode_from_str)
            .transpose()?
            .unwrap_or(base.mode),
        fan: patch
            .fan
            .as_deref()
            .map(fan_from_str)
            .transpose()?
            .unwrap_or(base.fan),
        set_temp_x2: patch.target_temp_c.map_or(base.set_temp_x2, temp_c_to_x2),
        myzone_id: patch.myzone_id.unwrap_or(base.myzone_id),
        fresh_air: match patch.fresh_air {
            Some(true) => FreshAir::On,
            Some(false) => FreshAir::Off,
            None => base.fresh_air,
        },
        rf_sys_id: base.rf_sys_id,
    };
    Ok(vec![CanRecord {
        unit_type,
        dest: Dest::ControlBox,
        unit_id,
        reg: RegId::new(0x05),
        data: status.into(),
    }])
}

fn encode_zone_state_update(
    bank: &RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
    payload: &Value,
) -> Result<Vec<CanRecord>, EncodeError> {
    let patch: ZoneStatePatch = deserialize_payload(payload)?;
    let zone_id = match patch.zone_id {
        ZoneIdJson::Num(n) => u8::try_from(n)
            .map_err(|_| EncodeError::BadPayload(format!("zone_id out of range: {n}")))?,
        ZoneIdJson::Str(s) => parse_zone_id_str(&s)?,
    };
    let base = match bank.get_zone_decoded(unit_type, unit_id, RegId::new(0x03), zone_id) {
        Some(DecodedRegister::ZoneState(state)) => state,
        _ => ZoneState {
            zone: zone_id,
            open: false,
            percent: 0,
            sensor: SensorType::NoSensor,
            set_temp_x2: 0,
            meas_int: 0,
            meas_dec: 0,
        },
    };
    let state = ZoneState {
        zone: zone_id,
        open: patch.open.unwrap_or(base.open),
        percent: patch.damper_pct.unwrap_or(base.percent),
        sensor: match patch.sensor_type.as_deref() {
            Some(s) => sensor_from_str(s)?,
            None => base.sensor,
        },
        set_temp_x2: patch.target_temp_c.map_or(base.set_temp_x2, temp_c_to_x2),
        meas_int: patch
            .measured_temp_c
            .map(measured_from_c)
            .map_or(base.meas_int, |(i, _)| i),
        meas_dec: patch
            .measured_temp_c
            .map(measured_from_c)
            .map_or(base.meas_dec, |(_, d)| d),
    };
    Ok(vec![CanRecord {
        unit_type,
        dest: Dest::ControlBox,
        unit_id,
        reg: RegId::new(0x03),
        data: state.into(),
    }])
}

/// Apply a slice of records into `bank` (last-write-wins per [`RegisterBank::apply`]).
pub fn apply_records_to_bank(bank: &mut RegisterBank, records: &[CanRecord]) {
    for record in records {
        bank.apply(record);
    }
}

/// Apply snapshot DTOs into `bank` as ControlBox-bound records.
///
/// Used for acceptance round-trips. Zone config is synthesized with header
/// `0x20` and zeroed constant-zone id slots (JSON only carries the count).
/// System status encodes `rf_sys_id = 0` (not present in JSON).
///
/// # Errors
///
/// Propagates [`EncodeError`] from DTO → wire enum mapping.
pub fn apply_snapshot_body_to_bank(
    bank: &mut RegisterBank,
    unit_type: UnitType,
    unit_id: UnitId,
    body: &SnapshotBody,
) -> Result<(), EncodeError> {
    if let Some(ref status) = body.system_status {
        let record = CanRecord {
            unit_type,
            dest: Dest::ControlBox,
            unit_id,
            reg: RegId::new(0x05),
            data: system_status_from_dto(status)?.into(),
        };
        bank.apply(&record);
    }
    if let Some(ref cfg) = body.zone_config {
        let record = CanRecord {
            unit_type,
            dest: Dest::ControlBox,
            unit_id,
            reg: RegId::new(0x01),
            data: zone_config_from_dto(cfg).into(),
        };
        bank.apply(&record);
    }
    if let Some(ref zones) = body.zones {
        for (key, dto) in zones {
            let zone_id = parse_zone_id_str(key)?;
            let record = CanRecord {
                unit_type,
                dest: Dest::ControlBox,
                unit_id,
                reg: RegId::new(0x03),
                data: zone_state_from_dto(zone_id, dto)?.into(),
            };
            bank.apply(&record);
        }
    }
    Ok(())
}

/// Map a typed [`SystemStatus`] to its JSON DTO.
#[must_use]
pub fn system_status_to_dto(status: &SystemStatus) -> SystemStatusDto {
    SystemStatusDto {
        power: power_to_str(status.power),
        mode: mode_to_str(status.mode),
        fan: fan_to_str(status.fan),
        target_temp_c: temp_x2_to_c(status.set_temp_x2),
        myzone_id: status.myzone_id,
        fresh_air: matches!(status.fresh_air, FreshAir::On),
    }
}

/// Map a system-status DTO to wire [`SystemStatus`] (`rf_sys_id` defaults to `0`).
///
/// # Errors
///
/// Returns [`EncodeError::BadEnum`] for unknown power/mode/fan strings.
pub fn system_status_from_dto(dto: &SystemStatusDto) -> Result<SystemStatus, EncodeError> {
    Ok(SystemStatus {
        power: power_from_str(&dto.power)?,
        mode: mode_from_str(&dto.mode)?,
        fan: fan_from_str(&dto.fan)?,
        set_temp_x2: temp_c_to_x2(dto.target_temp_c),
        myzone_id: dto.myzone_id,
        fresh_air: if dto.fresh_air {
            FreshAir::On
        } else {
            FreshAir::Off
        },
        rf_sys_id: 0,
    })
}

/// Map a typed [`ZoneConfig`] to its JSON DTO (header / constant ids omitted).
#[must_use]
pub const fn zone_config_to_dto(cfg: &ZoneConfig) -> ZoneConfigDto {
    ZoneConfigDto {
        total_zones: cfg.num_zones,
        constant_zones: cfg.num_constant,
        filter_clean_required: cfg.filter_clean,
    }
}

/// Synthesize wire [`ZoneConfig`] from a DTO (header `0x20`, constant ids zeroed).
#[must_use]
pub const fn zone_config_from_dto(dto: &ZoneConfigDto) -> ZoneConfig {
    ZoneConfig {
        header: ZONE_CONFIG_HEADER_CB_TO_TABLET,
        num_zones: dto.total_zones,
        num_constant: dto.constant_zones,
        constant: [0, 0, 0],
        filter_clean: dto.filter_clean_required,
    }
}

/// Map a typed [`ZoneState`] to its JSON DTO (zone id is the map key / event field).
#[must_use]
pub fn zone_dto_from_state(state: &ZoneState) -> ZoneDto {
    ZoneDto {
        open: state.open,
        damper_pct: state.percent,
        sensor_type: sensor_to_str(state.sensor),
        target_temp_c: temp_x2_to_c(state.set_temp_x2),
        measured_temp_c: measured_to_c(state.meas_int, state.meas_dec),
    }
}

/// Map a zone DTO + zone id to wire [`ZoneState`].
///
/// # Errors
///
/// Returns [`EncodeError::BadEnum`] for an unknown `sensor_type`.
pub fn zone_state_from_dto(zone_id: u8, dto: &ZoneDto) -> Result<ZoneState, EncodeError> {
    let (meas_int, meas_dec) = measured_from_c(dto.measured_temp_c);
    Ok(ZoneState {
        zone: zone_id,
        open: dto.open,
        percent: dto.damper_pct,
        sensor: sensor_from_str(&dto.sensor_type)?,
        set_temp_x2: temp_c_to_x2(dto.target_temp_c),
        meas_int,
        meas_dec,
    })
}

fn deserialize_payload<T: for<'de> Deserialize<'de>>(payload: &Value) -> Result<T, EncodeError> {
    serde_json::from_value(payload.clone()).map_err(|e| EncodeError::BadPayload(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ZoneIdJson {
    Num(u64),
    Str(String),
}

fn parse_zone_id_str(s: &str) -> Result<u8, EncodeError> {
    s.parse::<u8>()
        .map_err(|_| EncodeError::BadPayload(format!("invalid zone_id: {s}")))
}

fn temp_x2_to_c(set_temp_x2: u8) -> f64 {
    f64::from(set_temp_x2) / 2.0
}

fn temp_c_to_x2(temp_c: f64) -> u8 {
    // round(temp * 2); clamp to u8 domain.
    let scaled = (temp_c * 2.0).round();
    if scaled <= 0.0 {
        0
    } else if scaled >= f64::from(u8::MAX) {
        u8::MAX
    } else {
        // scaled is in (0, 255) after the guards above.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            scaled as u8
        }
    }
}

fn measured_to_c(meas_int: u8, meas_dec: u8) -> f64 {
    f64::from(meas_int) + f64::from(meas_dec) / 10.0
}

fn measured_from_c(temp_c: f64) -> (u8, u8) {
    let clamped = if temp_c < 0.0 { 0.0 } else { temp_c };
    let int_part = clamped.trunc();
    let frac = ((clamped - int_part) * 10.0).round();
    let meas_int = if int_part >= f64::from(u8::MAX) {
        u8::MAX
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            int_part as u8
        }
    };
    let meas_dec = if frac <= 0.0 {
        0
    } else if frac >= 9.0 {
        9
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            frac as u8
        }
    };
    (meas_int, meas_dec)
}

fn unknown_byte_label(value: u8) -> String {
    format!("unknown_{value:02x}")
}

fn power_to_str(power: Power) -> String {
    match power {
        Power::Off => "off".to_owned(),
        Power::On => "on".to_owned(),
        Power::Unknown(v) => unknown_byte_label(v),
    }
}

fn power_from_str(s: &str) -> Result<Power, EncodeError> {
    match s {
        "off" => Ok(Power::Off),
        "on" => Ok(Power::On),
        other => Err(EncodeError::BadEnum {
            field: "power",
            value: other.to_owned(),
        }),
    }
}

fn mode_to_str(mode: Mode) -> String {
    match mode {
        Mode::Cool => "cool".to_owned(),
        Mode::Heat => "heat".to_owned(),
        Mode::Vent => "vent".to_owned(),
        Mode::Auto => "auto".to_owned(),
        Mode::Dry => "dry".to_owned(),
        Mode::MyAuto => "my_auto".to_owned(),
        Mode::Unknown(v) => unknown_byte_label(v),
    }
}

fn mode_from_str(s: &str) -> Result<Mode, EncodeError> {
    match s {
        "cool" => Ok(Mode::Cool),
        "heat" => Ok(Mode::Heat),
        "vent" => Ok(Mode::Vent),
        "auto" => Ok(Mode::Auto),
        "dry" => Ok(Mode::Dry),
        "my_auto" | "myauto" => Ok(Mode::MyAuto),
        other => Err(EncodeError::BadEnum {
            field: "mode",
            value: other.to_owned(),
        }),
    }
}

fn fan_to_str(fan: Fan) -> String {
    match fan {
        Fan::Off => "off".to_owned(),
        Fan::Low => "low".to_owned(),
        Fan::Medium => "medium".to_owned(),
        Fan::High => "high".to_owned(),
        Fan::Auto => "auto".to_owned(),
        Fan::AutoAa => "auto_aa".to_owned(),
        Fan::Unknown(v) => unknown_byte_label(v),
    }
}

fn fan_from_str(s: &str) -> Result<Fan, EncodeError> {
    match s {
        "off" => Ok(Fan::Off),
        "low" => Ok(Fan::Low),
        "medium" => Ok(Fan::Medium),
        "high" => Ok(Fan::High),
        "auto" => Ok(Fan::Auto),
        "auto_aa" => Ok(Fan::AutoAa),
        other => Err(EncodeError::BadEnum {
            field: "fan",
            value: other.to_owned(),
        }),
    }
}

fn sensor_to_str(sensor: SensorType) -> String {
    match sensor {
        SensorType::NoSensor => "no_sensor".to_owned(),
        SensorType::Rf => "rf".to_owned(),
        SensorType::Wired => "wired".to_owned(),
        SensorType::Rf2CanBooster => "rf2can_booster".to_owned(),
        SensorType::RfX => "rf_x".to_owned(),
        SensorType::Unknown(v) => unknown_byte_label(v),
    }
}

fn sensor_from_str(s: &str) -> Result<SensorType, EncodeError> {
    match s {
        "no_sensor" => Ok(SensorType::NoSensor),
        "rf" => Ok(SensorType::Rf),
        // aaservice fixtures use "temp" for a wired temperature sensor.
        "wired" | "temp" => Ok(SensorType::Wired),
        "rf2can_booster" => Ok(SensorType::Rf2CanBooster),
        "rf_x" => Ok(SensorType::RfX),
        other => Err(EncodeError::BadEnum {
            field: "sensor_type",
            value: other.to_owned(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_registers::{
        CanRecord, Dest, Fan, FreshAir, Mode, Power, RegId, SensorType, SystemStatus, UnitId,
        UnitType, ZoneState,
    };
    use serde_json::json;

    fn unit_id() -> UnitId {
        UnitId::from_hex("abcde").unwrap()
    }

    fn seed_bank() -> RegisterBank {
        let mut bank = RegisterBank::new();
        let status = SystemStatus {
            power: Power::On,
            mode: Mode::Cool,
            fan: Fan::High,
            set_temp_x2: 0x30, // 24.0°C
            myzone_id: 1,
            fresh_air: FreshAir::Off,
            rf_sys_id: 0x42,
        };
        let cfg = ZoneConfig {
            header: 0x20,
            num_zones: 4,
            num_constant: 1,
            constant: [0x01, 0x00, 0x00],
            filter_clean: false,
        };
        let zone = ZoneState {
            zone: 1,
            open: true,
            percent: 100,
            sensor: SensorType::Wired,
            set_temp_x2: 45, // 22.5°C
            meas_int: 23,
            meas_dec: 1,
        };
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x05),
            data: status.into(),
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x01),
            data: cfg.into(),
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x03),
            data: zone.into(),
        });
        bank
    }

    #[test]
    fn snapshot_from_bank_maps_dto_fields() {
        let bank = seed_bank();
        let msg = snapshot_from_bank(&bank, UnitType::AIRCON, unit_id());
        let ServerMessage::MailboxSnapshot {
            unit_id,
            system_status,
            zone_config,
            zones,
            can_records,
        } = msg
        else {
            panic!("expected snapshot");
        };
        assert_eq!(unit_id, "abcde");
        let status = system_status.expect("system_status");
        assert_eq!(status.power, "on");
        assert_eq!(status.mode, "cool");
        assert_eq!(status.fan, "high");
        assert!((status.target_temp_c - 24.0).abs() < f64::EPSILON);
        assert_eq!(status.myzone_id, 1);
        assert!(!status.fresh_air);
        let cfg = zone_config.expect("zone_config");
        assert_eq!(cfg.total_zones, 4);
        assert_eq!(cfg.constant_zones, 1);
        assert!(!cfg.filter_clean_required);
        let zones = zones.expect("zones");
        assert_eq!(zones.len(), 1);
        let z1 = zones.get("1").expect("zone 1");
        assert!(z1.open);
        assert_eq!(z1.damper_pct, 100);
        assert_eq!(z1.sensor_type, "wired");
        assert!((z1.target_temp_c - 22.5).abs() < f64::EPSILON);
        assert!((z1.measured_temp_c - 23.1).abs() < f64::EPSILON);
        let can_records = can_records.expect("can_records");
        assert_eq!(can_records.len(), 3);
        assert!(can_records.iter().any(|r| r[9..11] == *"05"));
        assert!(can_records.iter().any(|r| r[9..11] == *"01"));
        assert!(can_records.iter().any(|r| r[9..11] == *"03"));
    }

    #[test]
    fn snapshot_includes_opaque_regs_in_can_records() {
        // Regression: MyAir5 rawCan needs the full dump (02/04/08/0a), not only DTOs.
        let mut bank = seed_bank();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x0a),
            data: [0; 7],
        });
        let body = snapshot_body_from_bank(&bank, UnitType::AIRCON, unit_id());
        let records = body.can_records.expect("can_records");
        assert!(records.iter().any(|r| &r[9..11] == "0a"));
        assert_eq!(records.len(), 4);
    }

    #[test]
    fn snapshot_dto_round_trip_through_bank() {
        let bank = seed_bank();
        let body = snapshot_body_from_bank(&bank, UnitType::AIRCON, unit_id());
        let mut bank2 = RegisterBank::new();
        apply_snapshot_body_to_bank(&mut bank2, UnitType::AIRCON, unit_id(), &body).unwrap();
        let body2 = snapshot_body_from_bank(&bank2, UnitType::AIRCON, unit_id());
        // Compare public DTO fields (rf_sys_id and zone-config constant ids are
        // not represented in JSON and may differ on the wire).
        assert_eq!(body.unit_id, body2.unit_id);
        assert_eq!(body.system_status, body2.system_status);
        assert_eq!(body.zone_config, body2.zone_config);
        assert_eq!(body.zones, body2.zones);
    }

    #[test]
    fn records_from_update_system_status() {
        let payload = json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false
        });
        let records =
            records_from_update(UnitType::AIRCON, unit_id(), "system_status", &payload).unwrap();
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.reg.get(), 0x05);
        assert_eq!(r.dest, Dest::ControlBox);
        assert_eq!(r.unit_id, unit_id());
        let DecodedRegister::SystemStatus(s) = r.decode() else {
            panic!("expected SystemStatus");
        };
        assert_eq!(s.power, Power::On);
        assert_eq!(s.mode, Mode::Cool);
        assert_eq!(s.fan, Fan::High);
        assert_eq!(s.set_temp_x2, 48);
        assert_eq!(s.fresh_air, FreshAir::Off);
        assert_eq!(s.rf_sys_id, 0);
    }

    #[test]
    fn records_from_update_zone_state() {
        let payload = json!({
            "zone_id": "1",
            "open": true,
            "damper_pct": 80,
            "sensor_type": "temp",
            "target_temp_c": 22.5,
            "measured_temp_c": 23.4
        });
        let records =
            records_from_update(UnitType::AIRCON, unit_id(), "zone_state", &payload).unwrap();
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.reg.get(), 0x03);
        assert_eq!(r.dest, Dest::ControlBox);
        let DecodedRegister::ZoneState(z) = r.decode() else {
            panic!("expected ZoneState");
        };
        assert_eq!(z.zone, 1);
        assert!(z.open);
        assert_eq!(z.percent, 80);
        assert_eq!(z.sensor, SensorType::Wired);
        assert_eq!(z.set_temp_x2, 45);
        assert_eq!(z.meas_int, 23);
        assert_eq!(z.meas_dec, 4);
    }

    #[test]
    fn records_from_update_with_bank_merges_partial_system_status() {
        // aaservice setAircon sends sparse payloads (no myzone_id/fresh_air);
        // the merge must preserve the bank's current values for absent fields.
        let mut bank = RegisterBank::new();
        let base = SystemStatus {
            power: Power::On,
            mode: Mode::Cool,
            fan: Fan::Auto,
            set_temp_x2: 0x30,
            myzone_id: 3,
            fresh_air: FreshAir::On,
            rf_sys_id: 0x42,
        };
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x05),
            data: base.into(),
        });
        let payload = json!({"power": "off", "target_temp_c": 25.0});
        let records = records_from_update_with_bank(
            &bank,
            UnitType::AIRCON,
            unit_id(),
            "system_status",
            &payload,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let DecodedRegister::SystemStatus(s) = records[0].decode() else {
            panic!("expected SystemStatus");
        };
        assert_eq!(s.power, Power::Off);
        assert_eq!(s.mode, Mode::Cool); // preserved from bank
        assert_eq!(s.fan, Fan::Auto); // preserved from bank
        assert_eq!(s.set_temp_x2, 50);
        assert_eq!(s.myzone_id, 3); // preserved from bank
        assert_eq!(s.fresh_air, FreshAir::On); // preserved from bank
        assert_eq!(s.rf_sys_id, 0x42); // preserved from bank
    }

    #[test]
    fn records_from_update_with_bank_merges_partial_zone_state() {
        let mut bank = RegisterBank::new();
        let base = ZoneState {
            zone: 2,
            open: true,
            percent: 80,
            sensor: SensorType::Wired,
            set_temp_x2: 45,
            meas_int: 22,
            meas_dec: 5,
        };
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: unit_id(),
            reg: RegId::new(0x03),
            data: base.into(),
        });
        let payload = json!({"zone_id": "2", "open": false});
        let records = records_from_update_with_bank(
            &bank,
            UnitType::AIRCON,
            unit_id(),
            "zone_state",
            &payload,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let DecodedRegister::ZoneState(z) = records[0].decode() else {
            panic!("expected ZoneState");
        };
        assert!(!z.open);
        assert_eq!(z.percent, 80); // preserved from bank
        assert_eq!(z.sensor, SensorType::Wired); // preserved from bank
        assert_eq!(z.set_temp_x2, 45); // preserved from bank
        assert_eq!(z.meas_int, 22); // preserved from bank
        assert_eq!(z.meas_dec, 5); // preserved from bank
    }

    #[test]
    fn unsupported_register_errors() {
        let payload = json!({});
        let err =
            records_from_update(UnitType::AIRCON, unit_id(), "zone_config", &payload).unwrap_err();
        assert!(matches!(err, EncodeError::UnsupportedRegister(_)));
        let err = records_from_update(UnitType::AIRCON, unit_id(), "unit_activation", &payload)
            .unwrap_err();
        assert!(matches!(err, EncodeError::UnsupportedRegister(_)));
    }

    #[test]
    fn bad_enum_on_write() {
        let payload = json!({
            "power": "maybe",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false
        });
        let err = records_from_update(UnitType::AIRCON, unit_id(), "system_status", &payload)
            .unwrap_err();
        assert!(matches!(err, EncodeError::BadEnum { field: "power", .. }));
    }
}
