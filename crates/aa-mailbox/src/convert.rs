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
/// `can_records` (CB dump hex for MyAir5 rawCan; excludes synthesized regs).
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
    match register {
        "system_status" => {
            let dto: SystemStatusDto = deserialize_payload(payload)?;
            let status = system_status_from_dto(&dto)?;
            Ok(vec![CanRecord {
                unit_type,
                dest: Dest::ControlBox,
                unit_id,
                reg: RegId::new(0x05),
                data: status.into(),
            }])
        }
        "zone_state" => {
            let (zone_id, dto) = parse_zone_state_payload(payload)?;
            let state = zone_state_from_dto(zone_id, &dto)?;
            Ok(vec![CanRecord {
                unit_type,
                dest: Dest::ControlBox,
                unit_id,
                reg: RegId::new(0x03),
                data: state.into(),
            }])
        }
        other => Err(EncodeError::UnsupportedRegister(other.to_owned())),
    }
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

fn parse_zone_state_payload(payload: &Value) -> Result<(u8, ZoneDto), EncodeError> {
    #[derive(Deserialize)]
    struct ZoneStateUpdate {
        zone_id: ZoneIdJson,
        open: bool,
        damper_pct: u8,
        sensor_type: String,
        target_temp_c: f64,
        measured_temp_c: f64,
    }

    let update: ZoneStateUpdate = deserialize_payload(payload)?;
    let zone_id = match update.zone_id {
        ZoneIdJson::Num(n) => {
            if n > u64::from(u8::MAX) {
                return Err(EncodeError::BadPayload(format!(
                    "zone_id out of range: {n}"
                )));
            }
            u8::try_from(n)
                .map_err(|_| EncodeError::BadPayload(format!("zone_id out of range: {n}")))?
        }
        ZoneIdJson::Str(s) => parse_zone_id_str(&s)?,
    };
    Ok((
        zone_id,
        ZoneDto {
            open: update.open,
            damper_pct: update.damper_pct,
            sensor_type: update.sensor_type,
            target_temp_c: update.target_temp_c,
            measured_temp_c: update.measured_temp_c,
        },
    ))
}

#[derive(Deserialize)]
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
        CanRecord, Dest, Fan, FreshAir, Mode, Power, RegId, SensorType, UnitId, UnitType,
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
