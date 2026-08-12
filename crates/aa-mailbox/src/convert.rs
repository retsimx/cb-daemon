//! Hybrid DTO ↔ wire codec for the register catalog.
//!
//! Two entry points convert between northbound JSON payloads and the 7-byte
//! CAN2 register payloads:
//!
//! - [`encode_payload`]: typed DTO object → wire bytes, or a raw 14-char hex
//!   string → wire bytes (byte-exact passthrough, accepted for known and
//!   unknown registers alike).
//! - [`decode_payload`]: wire bytes → typed DTO object for known registers, or
//!   a raw 14-char hex string for unknown registers.
//!
//! The per-register wire codecs live in the [`codec`] submodule, one section
//! per register; this module owns the dispatch and the shared helpers they
//! build on (hex / temperature / DTO (de)serialization).
//!
//! The codec is hybrid by design (design D-1):
//!
//! - Every catalogued register (`01`–`06`, `08`–`0a`, `12`, `13`, `26`, `27`)
//!   routes through the [`aa_registers`] typed structs (`ZoneConfig` /
//!   `UnitActivation` / `ZoneState` / `ZoneLimits` / `SystemStatus` /
//!   `FirmwareStatus` / `SystemError` / `ActivationCode` / `UnitAnnouncement` /
//!   `SensorPairingRead`/`SensorPairingWrite` / `InfoByte` / `RfDevicePairing` /
//!   `RfDeviceCalibration`) and their `From<[u8; 7]>` / `Into<[u8; 7]>` impls.
//! - Unknown registers have no DTO and only support raw-hex passthrough.
//!
//! # Zone bytes (`03`, `04`)
//!
//! The zone id is part of the CAN address, not the payload, and neither
//! [`encode_payload`] nor [`decode_payload`] carries a zone parameter.
//! Encode stamps the zone byte `0x00`; the daemon bridge (`cb-daemon`
//! `ws.rs`) overwrites it with the client-addressed zone for zone-bearing
//! writes. Decode drops the zone byte from the DTO; the bridge reads it back
//! from `data[0]` for events.
//!
//! # Raw-hex escape hatch
//!
//! Any register accepts a 14-char hex string on encode (byte-exact). On decode,
//! an unknown register — or a known register whose wire carries an enum byte the
//! exhaustive DTO enums cannot represent (`02` unit type / activation status,
//! `03` sensor type, `05` power/mode/fan/fresh-air, `09` action) — falls back
//! to the raw 14-char lowercase hex string instead of a lossy typed object.
//!
//! # Documented wire enum mappings (normative from `aa_interop`)
//!
//! - Reg `02` `unit_type`: `daikin`=0x11, `panasonic`=0x12, `fujitsu`=0x13,
//!   `samsung_dvm`=0x19 (via the [`aa_registers::UnitBrand`] enum). This
//!   register is read-only per the epic write policy, but the codec still
//!   supports decode.
//! - Reg `02` `activation_status`: `no_code`=0, `code_enabled`=1, `expired`=2.
//! - Reg `09` `action`: `set_code`=1, `unlock`=2 (via the
//!   [`aa_registers::Action`] enum).
//! - Reg `12` pairing: bit 6 (`0x40`) of byte 3 (the read shape's info byte);
//!   any byte with bit 6 unset decodes to `pairing: false`.
//! - Reg `05` `fresh_air`: DTO `true` → `FreshAir::On` (`0x02`), `false` →
//!   `FreshAir::Off` (`0x01`). Decode maps `On` → `true` and `Off`/`None`
//!   (`0x00`, "no fresh-air hardware") → `false` — `None` cannot be represented
//!   in the bool DTO and is conflated with `Off` (matches the legacy mapper).
//!
//! # Reg `12` read vs write shape
//!
//! The wire layouts differ only in byte 3 semantics (`info` vs `zone`) and byte
//! 4 (`rev` vs `0`), which are not distinguishable from the bytes alone. On the
//! read shape byte 3 is the raw info byte with bit 6 (`0x40`) meaning
//! "pairing requested" (see the wire enum mappings above). Encode tries the
//! read shape ([`SensorPairingDto`], carries `pairing`/`sensor_rev`) first and
//! falls back to the write shape ([`SensorPairingWriteDto`], carries `zone`);
//! a payload that matches both is treated as a read. Decode always returns the
//! read shape — a write echo's zone is not recoverable.

mod codec;

use std::collections::BTreeMap;

use aa_registers::{CanRecord, RegId, RegisterBank, ZONE_BEARING_REGS, is_zone_bearing};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tracing::debug;

use crate::error::{EncodeError, WriteError};
use crate::policy::{PolicyMode, write_policy};

use self::codec::{
    decode_activation_code, decode_firmware, decode_info_byte, decode_rf_device_calibration,
    decode_rf_device_pairing, decode_sensor_pairing, decode_system_error, decode_system_status,
    decode_unit_activation, decode_unit_announcement, decode_zone_config, decode_zone_limits,
    decode_zone_state, encode_activation_code, encode_firmware, encode_info_byte,
    encode_rf_device_calibration, encode_rf_device_pairing, encode_sensor_pairing,
    encode_system_error, encode_system_status, encode_unit_activation, encode_unit_announcement,
    encode_zone_config, encode_zone_limits, encode_zone_state,
};

/// Encode a register payload into 7 wire bytes.
///
/// `payload` is either a typed DTO object for the register (converted per the
/// register's wire layout) or a raw 14-char hex string (byte-exact
/// passthrough, accepted for known and unknown registers alike).
///
/// Zone-bearing registers (`03`, `04`) stamp the zone byte `0x00`: the zone is
/// part of the CAN address, and this entry point carries no zone parameter
/// (the daemon bridge overwrites it with the addressed zone; see module docs).
///
/// # Errors
///
/// Returns [`EncodeError::UnknownRegister`] for registers outside the catalog
/// with a non-hex payload, [`EncodeError::BadHex`] for malformed hex strings
/// (wrong length or non-hex digits), [`EncodeError::BadEnum`] for unrecognised
/// enum string values, and [`EncodeError::BadPayload`] for payloads that do not
/// deserialize into the register's DTO.
pub fn encode_payload(reg: RegId, payload: &Value) -> Result<[u8; 7], EncodeError> {
    if let Some(hex) = payload.as_str() {
        return hex_to_bytes(hex);
    }
    match reg.get() {
        0x01 => encode_zone_config(payload),
        0x02 => encode_unit_activation(payload),
        0x03 => encode_zone_state(payload),
        0x04 => encode_zone_limits(payload),
        0x05 => encode_system_status(payload),
        0x06 => encode_firmware(payload),
        0x08 => encode_system_error(payload),
        0x09 => encode_activation_code(payload),
        0x0a => encode_unit_announcement(payload),
        0x12 => encode_sensor_pairing(payload),
        0x13 => encode_info_byte(payload),
        0x26 => encode_rf_device_pairing(payload),
        0x27 => encode_rf_device_calibration(payload),
        _ => Err(EncodeError::UnknownRegister(reg.to_string())),
    }
}

/// Decode 7 wire bytes into a typed DTO object for known registers, or a raw
/// 14-char lowercase hex string for unknown registers (`16`/`17`/`1d`/`1e`).
///
/// Registers whose DTO enums cannot represent a wire byte (`02` unit type /
/// activation status, `03` sensor type, `05` power/mode/fan/fresh-air, `09`
/// action) fall back to the raw hex string instead of returning a lossy typed
/// object. Zone bytes (`03`, `04`) are part of the CAN address, not the
/// payload, and are dropped from the DTO output.
///
/// # Errors
///
/// Returns [`EncodeError::BadPayload`] if the decoded DTO cannot be serialized
/// back to JSON (unreachable for the built-in catalog).
pub fn decode_payload(reg: RegId, data: [u8; 7]) -> Result<Value, EncodeError> {
    match reg.get() {
        0x01 => decode_zone_config(data),
        0x02 => decode_unit_activation(data),
        0x03 => decode_zone_state(data),
        0x04 => decode_zone_limits(data),
        0x05 => decode_system_status(data),
        0x06 => decode_firmware(data),
        0x08 => decode_system_error(data),
        0x09 => decode_activation_code(data),
        0x0a => decode_unit_announcement(data),
        0x12 => decode_sensor_pairing(data),
        0x13 => decode_info_byte(data),
        0x26 => decode_rf_device_pairing(data),
        0x27 => decode_rf_device_calibration(data),
        _ => Ok(Value::String(bytes_to_hex(data))),
    }
}

/// Maximum zone id scanned for zone-bearing registers in a snapshot.
const MAX_ZONE: u8 = 10;

/// A single wire-range bound for a register (issue #32).
struct WireRange {
    /// Wire byte index per the register layout.
    index: usize,
    /// Display field name (issue #32).
    field: &'static str,
    /// Inclusive maximum wire value.
    max: u8,
    /// Bit mask applied to the wire byte before the comparison.
    mask: u8,
}

/// Per-register wire-range table (issue #32). Only regs `01`/`03`/`04`/`05`
/// carry ranges; all other registers skip the range check.
const WIRE_RANGES: &[(u8, &[WireRange])] = &[
    (
        0x01,
        &[
            WireRange {
                index: 1,
                field: "numZones",
                max: 10,
                mask: 0xff,
            },
            WireRange {
                index: 2,
                field: "numConstants",
                max: 3,
                mask: 0xff,
            },
        ],
    ),
    (
        0x03,
        &[
            WireRange {
                index: 1,
                field: "damper",
                max: 100,
                mask: 0x7f,
            },
            WireRange {
                index: 3,
                field: "setTemp×2",
                max: 80,
                mask: 0xff,
            },
            WireRange {
                index: 5,
                field: "measDec",
                max: 9,
                mask: 0xff,
            },
        ],
    ),
    (
        0x04,
        &[
            WireRange {
                index: 3,
                field: "motion",
                max: 22,
                mask: 0xff,
            },
            WireRange {
                index: 4,
                field: "motionConfig",
                max: 2,
                mask: 0xff,
            },
        ],
    ),
    (
        0x05,
        &[
            WireRange {
                index: 1,
                field: "mode",
                max: 6,
                mask: 0xff,
            },
            WireRange {
                index: 3,
                field: "setTemp×2",
                max: 80,
                mask: 0xff,
            },
            WireRange {
                index: 5,
                field: "freshAir",
                max: 2,
                mask: 0xff,
            },
            WireRange {
                index: 6,
                field: "rfSysId",
                max: 16,
                mask: 0xff,
            },
            WireRange {
                index: 4,
                field: "myzone",
                max: 10,
                mask: 0xff,
            },
        ],
    ),
];

/// Wire ranges for a register id (empty when the register has none).
fn wire_ranges(reg: u8) -> &'static [WireRange] {
    match WIRE_RANGES.iter().find(|(r, _)| *r == reg) {
        Some((_, ranges)) => ranges,
        None => &[],
    }
}

/// Check wire bytes against the register's range table; first violation wins.
fn check_ranges(reg: u8, data: [u8; 7]) -> Result<(), WriteError> {
    for range in wire_ranges(reg) {
        let value = data[range.index] & range.mask;
        if value > range.max {
            return Err(WriteError::OutOfRange {
                field: range.field,
                value,
                max: range.max,
            });
        }
    }
    Ok(())
}

/// Validate a register write against the D-4 write policy and the wire ranges
/// (issue #32).
///
/// Precedence: mode → field → range. Raw-hex payloads bypass the field and
/// range checks (client responsibility) but still fail the mode check. A typed
/// payload that fails to encode skips the range check — the outer write path
/// surfaces the real [`EncodeError`].
///
/// # Errors
///
/// Returns [`WriteError::ReadOnlyRegister`] / [`WriteError::InternalRegister`]
/// / [`WriteError::UnverifiedRegister`] for non-writable registers,
/// [`WriteError::ReadOnlyField`] for a typed payload carrying a read-only
/// field, and [`WriteError::OutOfRange`] for a wire value above its bound.
pub fn validate_write(reg: RegId, payload: &Value) -> Result<(), WriteError> {
    let policy = write_policy(reg);
    match policy.mode {
        PolicyMode::ReadOnly => return Err(WriteError::ReadOnlyRegister { reg: reg.get() }),
        PolicyMode::Internal => return Err(WriteError::InternalRegister { reg: reg.get() }),
        PolicyMode::Unverified => return Err(WriteError::UnverifiedRegister { reg: reg.get() }),
        PolicyMode::WriteOnly | PolicyMode::ReadWrite => {}
    }
    if payload.as_str().is_some() {
        return Ok(());
    }
    if let Some(obj) = payload.as_object() {
        for key in obj.keys() {
            if let Some(field) = policy
                .read_only_fields
                .iter()
                .copied()
                .find(|f| *f == key.as_str())
            {
                return Err(WriteError::ReadOnlyField {
                    reg: reg.get(),
                    field,
                });
            }
        }
    }
    let Ok(data) = encode_payload(reg, payload) else {
        return Ok(());
    };
    check_ranges(reg.get(), data)
}

/// Build the multi-unit snapshot body from a register bank.
///
/// Keyed by `"{unit_type}:{unit_id}"` via the [`std::fmt::Display`] impls
/// (2-hex type, 5-hex id, lowercase — e.g. `"07:181f3"`); each value is that
/// unit's register map. Non-zone registers decode to typed DTOs (or raw
/// 14-char hex for unknown registers); zone-bearing registers (`03`/`04`)
/// are nested zone → DTO maps, inserted only when non-empty. Registers with no
/// bank slot are skipped.
pub fn snapshot_units(bank: &RegisterBank) -> BTreeMap<String, BTreeMap<String, Value>> {
    let mut units = BTreeMap::new();
    for unit_type in bank.unit_types() {
        let records = bank.records_for_any_unit(unit_type);
        for unit_id in bank.unit_ids(unit_type) {
            let mut registers = BTreeMap::new();
            for record in &records {
                if record.unit_id != unit_id || is_zone_bearing(record.reg) {
                    continue;
                }
                match decode_payload(record.reg, record.data) {
                    Ok(payload) => {
                        registers.insert(format!("{:02x}", record.reg.get()), payload);
                    }
                    Err(err) => debug!(%err, "snapshot: skipping undecodable register"),
                }
            }
            for &reg_id in ZONE_BEARING_REGS {
                let reg = RegId::new(reg_id);
                let mut zones: BTreeMap<String, Value> = BTreeMap::new();
                for zone in 1..=MAX_ZONE {
                    if let Some(data) = bank.get_zone(unit_type, unit_id, reg, zone)
                        && let Ok(payload) = decode_payload(reg, data)
                    {
                        zones.insert(zone.to_string(), payload);
                    }
                }
                if !zones.is_empty() {
                    registers.insert(
                        format!("{reg_id:02x}"),
                        Value::Object(zones.into_iter().collect()),
                    );
                }
            }
            units.insert(format!("{unit_type}:{unit_id}"), registers);
        }
    }
    units
}

/// Build the per-unit event fields `(register_hex, zone, payload)` for a wire
/// record.
///
/// `zone` is `Some(record.data[0])` for zone-bearing registers (`03`/`04`);
/// otherwise `None`. Undecodable payloads fall back to the raw 14-char
/// lowercase hex string (same fallback as [`decode_payload`] for unknown
/// registers).
#[must_use]
pub fn event_body(record: &CanRecord) -> (String, Option<u8>, Value) {
    let register = format!("{:02x}", record.reg.get());
    let zone = is_zone_bearing(record.reg).then_some(record.data[0]);
    let payload = decode_payload(record.reg, record.data)
        .unwrap_or_else(|_| Value::String(bytes_to_hex(record.data)));
    (register, zone, payload)
}

// --- Shared helpers ----------------------------------------------------------

/// Deserialize a payload into a DTO, mapping serde failures to
/// [`EncodeError::BadPayload`].
fn deserialize_payload<T: DeserializeOwned>(payload: &Value) -> Result<T, EncodeError> {
    serde_json::from_value(payload.clone()).map_err(|e| EncodeError::BadPayload(e.to_string()))
}

/// Serialize a DTO into a JSON value (infallible for the catalog DTOs; the
/// error path is unreachable but required to avoid `unwrap`).
fn dto_to_value<T: Serialize>(dto: &T) -> Result<Value, EncodeError> {
    serde_json::to_value(dto).map_err(|e| EncodeError::BadPayload(e.to_string()))
}

/// Validate an enum-string field in a payload, producing
/// [`EncodeError::BadEnum`] with the offending field name instead of a generic
/// [`EncodeError::BadPayload`]. Validation goes through the serde enum itself,
/// so the accepted strings always match the DTO's `#[serde(rename_all)]`
/// config.
fn check_enum<E: DeserializeOwned>(
    field: &'static str,
    payload: &Value,
) -> Result<(), EncodeError> {
    let Some(value) = payload.get(field).and_then(Value::as_str) else {
        return Ok(());
    };
    if serde_json::from_value::<E>(Value::String(value.to_owned())).is_err() {
        return Err(EncodeError::BadEnum {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Parse a 14-char hex string into 7 wire bytes.
///
/// # Errors
///
/// Returns [`EncodeError::BadHex`] if `hex` is not exactly 14 hex characters.
fn hex_to_bytes(hex: &str) -> Result<[u8; 7], EncodeError> {
    if hex.len() != 14 || !hex.is_ascii() {
        return Err(EncodeError::BadHex(hex.to_owned()));
    }
    let bytes = hex.as_bytes();
    let mut data = [0u8; 7];
    for (i, byte) in data.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i]).ok_or_else(|| EncodeError::BadHex(hex.to_owned()))?;
        let lo = hex_nibble(bytes[2 * i + 1]).ok_or_else(|| EncodeError::BadHex(hex.to_owned()))?;
        *byte = (hi << 4) | lo;
    }
    Ok(data)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Format bytes as a lowercase hex string (`[0x01, 0x02]` → `"0102"`).
fn slice_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn bytes_to_hex(data: [u8; 7]) -> String {
    slice_to_hex(&data)
}

/// `target_temp_c` (°C) → wire `set_temp_x2` (degC × 2), rounded and clamped
/// to the `u8` domain.
fn temp_c_to_x2(temp_c: f64) -> u8 {
    let scaled = (temp_c * 2.0).round();
    if scaled <= 0.0 {
        0
    } else if scaled >= f64::from(u8::MAX) {
        u8::MAX
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            scaled as u8
        }
    }
}

/// Wire `set_temp_x2` → `target_temp_c` (°C).
fn temp_x2_to_c(set_temp_x2: u8) -> f64 {
    f64::from(set_temp_x2) / 2.0
}

/// Wire `meas_int` / `meas_dec` → measured temperature (°C).
fn measured_to_c(meas_int: u8, meas_dec: u8) -> f64 {
    f64::from(meas_int) + f64::from(meas_dec) / 10.0
}

/// Measured temperature (°C) → wire `(meas_int, meas_dec)`, clamped to
/// `0..=255` / `0..=9`.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{check_ranges, validate_write};
    use crate::error::WriteError;
    use aa_registers::RegId;
    use serde_json::{Value, json};

    fn raw_hex(hex: &str) -> Value {
        Value::String(hex.to_owned())
    }

    #[test]
    fn typed_write_to_read_only_register_rejected() {
        let err = validate_write(RegId::new(0x08), &json!({})).unwrap_err();
        assert_eq!(err, WriteError::ReadOnlyRegister { reg: 0x08 });
    }

    #[test]
    fn raw_hex_write_to_read_only_register_rejected() {
        let err = validate_write(RegId::new(0x08), &raw_hex("41413100000000")).unwrap_err();
        assert_eq!(err, WriteError::ReadOnlyRegister { reg: 0x08 });
    }

    #[test]
    fn typed_write_with_read_only_field_rejected() {
        let err = validate_write(
            RegId::new(0x03),
            &json!({ "open": true, "damper_pct": 50, "measured_temp_c": 23.1 }),
        )
        .unwrap_err();
        assert_eq!(
            err,
            WriteError::ReadOnlyField {
                reg: 0x03,
                field: "measured_temp_c",
            }
        );
    }

    #[test]
    fn typed_write_with_only_writable_fields_accepted() {
        let result = validate_write(
            RegId::new(0x03),
            &json!({ "open": true, "damper_pct": 50, "target_temp_c": 24.0 }),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn raw_hex_write_to_writable_register_accepted() {
        let result = validate_write(RegId::new(0x05), &raw_hex("01010330000100"));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn write_to_internal_register_rejected() {
        let err = validate_write(RegId::new(0x07), &json!({})).unwrap_err();
        assert_eq!(err, WriteError::InternalRegister { reg: 0x07 });
    }

    #[test]
    fn write_to_unverified_register_rejected() {
        let err = validate_write(RegId::new(0x1e), &json!({})).unwrap_err();
        assert_eq!(err, WriteError::UnverifiedRegister { reg: 0x1e });
    }

    #[test]
    fn typed_write_to_write_only_register_accepted() {
        let result = validate_write(
            RegId::new(0x09),
            &json!({ "action": "set_code", "unlock_code": "abcd", "activation_days": 30 }),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn reg01_typed_write_range_via_validate_write() {
        let payload = |zones: u8| {
            json!({
                "header": 0x20,
                "total_zones": zones,
                "constant_zones": 1,
                "constant_zone_ids": [1, 0, 0],
                "filter_clean_required": false,
            })
        };
        assert_eq!(
            validate_write(RegId::new(0x01), &payload(10)),
            Ok(()),
            "numZones at bound accepted"
        );
        let err = validate_write(RegId::new(0x01), &payload(11)).unwrap_err();
        assert_eq!(
            err,
            WriteError::OutOfRange {
                field: "numZones",
                value: 11,
                max: 10,
            }
        );
    }

    #[test]
    fn raw_hex_write_bypasses_range_checks() {
        // Mode byte 7 is out of range but raw-hex writes skip the range check.
        let result = validate_write(RegId::new(0x05), &raw_hex("01070000000000"));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn range_boundaries_reg01() {
        assert_eq!(check_ranges(0x01, [0, 10, 3, 0, 0, 0, 0]), Ok(()));
        assert_eq!(
            check_ranges(0x01, [0, 11, 3, 0, 0, 0, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "numZones",
                value: 11,
                max: 10
            }
        );
        assert_eq!(
            check_ranges(0x01, [0, 10, 4, 0, 0, 0, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "numConstants",
                value: 4,
                max: 3
            }
        );
    }

    #[test]
    fn range_boundaries_reg03() {
        assert_eq!(check_ranges(0x03, [0, 100, 0, 80, 0, 9, 0]), Ok(()));
        assert_eq!(
            check_ranges(0x03, [0, 101, 0, 80, 0, 9, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "damper",
                value: 101,
                max: 100
            }
        );
        assert_eq!(
            check_ranges(0x03, [0, 0xe4, 0, 80, 0, 9, 0]),
            Ok(()),
            "damper 100 with the open flag (bit 7) set is accepted"
        );
        assert_eq!(
            check_ranges(0x03, [0, 0xe5, 0, 80, 0, 9, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "damper",
                value: 101,
                max: 100
            }
        );
        assert_eq!(
            check_ranges(0x03, [0, 100, 0, 81, 0, 9, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "setTemp×2",
                value: 81,
                max: 80
            }
        );
        assert_eq!(
            check_ranges(0x03, [0, 100, 0, 80, 0, 10, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "measDec",
                value: 10,
                max: 9
            }
        );
    }

    #[test]
    fn range_boundaries_reg04() {
        assert_eq!(check_ranges(0x04, [0, 0, 0, 22, 2, 0, 0]), Ok(()));
        assert_eq!(
            check_ranges(0x04, [0, 0, 0, 23, 2, 0, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "motion",
                value: 23,
                max: 22
            }
        );
        assert_eq!(
            check_ranges(0x04, [0, 0, 0, 22, 3, 0, 0]).unwrap_err(),
            WriteError::OutOfRange {
                field: "motionConfig",
                value: 3,
                max: 2
            }
        );
    }

    #[test]
    fn range_boundaries_reg05() {
        assert_eq!(check_ranges(0x05, [0, 6, 0, 80, 10, 2, 16]), Ok(()));
        assert_eq!(
            check_ranges(0x05, [0, 7, 0, 80, 10, 2, 16]).unwrap_err(),
            WriteError::OutOfRange {
                field: "mode",
                value: 7,
                max: 6
            }
        );
        assert_eq!(
            check_ranges(0x05, [0, 6, 0, 81, 10, 2, 16]).unwrap_err(),
            WriteError::OutOfRange {
                field: "setTemp×2",
                value: 81,
                max: 80
            }
        );
        assert_eq!(
            check_ranges(0x05, [0, 6, 0, 80, 10, 3, 16]).unwrap_err(),
            WriteError::OutOfRange {
                field: "freshAir",
                value: 3,
                max: 2
            }
        );
        assert_eq!(
            check_ranges(0x05, [0, 6, 0, 80, 10, 2, 17]).unwrap_err(),
            WriteError::OutOfRange {
                field: "rfSysId",
                value: 17,
                max: 16
            }
        );
        assert_eq!(
            check_ranges(0x05, [0, 6, 0, 80, 11, 2, 16]).unwrap_err(),
            WriteError::OutOfRange {
                field: "myzone",
                value: 11,
                max: 10
            }
        );
    }

    #[test]
    fn ranges_skipped_for_registers_without_table() {
        assert_eq!(check_ranges(0x09, [0xff; 7]), Ok(()));
        assert_eq!(check_ranges(0x1e, [0xff; 7]), Ok(()));
    }
}
