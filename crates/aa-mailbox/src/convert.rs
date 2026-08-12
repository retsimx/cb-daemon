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
//! - Regs `01`/`03`/`05`/`06` reuse the [`aa_registers`] typed structs
//!   (`ZoneConfig` / `ZoneState` / `SystemStatus` / `FirmwareStatus`) and their
//!   existing `From<[u8; 7]>` / `Into<[u8; 7]>` impls.
//! - Regs `02`/`04`/`08`/`09`/`0a`/`12`/`13`/`26`/`27` use direct byte codecs
//!   per the normative wire layouts.
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
//! # Documented wire enum mappings (not normative in issue #27)
//!
//! - Reg `02` `unit_type`: `daikin`=0, `panasonic`=1, `fujitsu`=2,
//!   `samsung_dvm`=3. This register is read-only per the epic write policy, but
//!   the codec still supports decode.
//! - Reg `02` `activation_status`: `no_code`=0, `code_enabled`=1, `expired`=2.
//! - Reg `09` `action`: `set_code`=0, `unlock`=1.
//! - Reg `12` pairing byte: `true`=1, `false`=0 (read shape byte 3).
//! - Reg `05` `fresh_air`: DTO `true` → `FreshAir::On` (`0x02`), `false` →
//!   `FreshAir::Off` (`0x01`). Decode maps `On` → `true` and `Off`/`None`
//!   (`0x00`, "no fresh-air hardware") → `false` — `None` cannot be represented
//!   in the bool DTO and is conflated with `Off` (matches the legacy mapper).
//!
//! # Reg `12` read vs write shape
//!
//! The wire layouts differ only in byte 3 semantics (`info` vs `zone`) and byte
//! 4 (`rev` vs `0`), which are not distinguishable from the bytes alone. Encode
//! tries the read shape ([`SensorPairingDto`], carries `pairing`/`sensor_rev`)
//! first and falls back to the write shape ([`SensorPairingWriteDto`], carries
//! `zone`); a payload that matches both is treated as a read. Decode always
//! returns the read shape — a write echo's zone is not recoverable.

mod codec;

use aa_registers::RegId;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::EncodeError;

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
