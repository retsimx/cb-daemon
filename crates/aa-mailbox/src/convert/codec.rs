//! Per-register wire codecs (one section per register).
//!
//! Called from the [`super::encode_payload`] / [`super::decode_payload`]
//! dispatch in the parent module, which owns the shared helpers
//! (hex / temperature / DTO (de)serialization). Register-specific helpers
//! (`zone_state_to_wire`, `sensor_uid_to_bytes`, `hex2_to_byte`, the enum byte
//! mappers) stay here next to their register.

use aa_registers::{
    Fan, FirmwareStatus, FreshAir, Mode, Power, SensorType, SystemStatus, ZoneConfig, ZoneState,
};
use serde_json::Value;

use super::{
    bytes_to_hex, check_enum, deserialize_payload, dto_to_value, hex_nibble, measured_from_c,
    measured_to_c, slice_to_hex, temp_c_to_x2, temp_x2_to_c,
};
use crate::dto::{
    ActionEnum, ActivationCodeDto, ActivationStatus, FanEnum, FirmwareDto, InfoByteDto, ModeEnum,
    PowerEnum, RfDeviceCalibrationDto, RfDevicePairingDto, SensorPairingDto, SensorPairingWriteDto,
    SensorTypeEnum, SystemErrorDto, SystemStatusDto, UnitActivationDto, UnitAnnouncementDto,
    UnitTypeEnum, ZoneConfigDto, ZoneLimitsDto, ZoneStateDto,
};
use crate::error::EncodeError;

// --- Reg 01: zone config (aa-registers `ZoneConfig`) ------------------------

pub(super) fn encode_zone_config(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: ZoneConfigDto = deserialize_payload(payload)?;
    Ok(ZoneConfig {
        header: dto.header,
        num_zones: dto.total_zones,
        num_constant: dto.constant_zones,
        constant: dto.constant_zone_ids,
        filter_clean: dto.filter_clean_required,
    }
    .into())
}

pub(super) fn decode_zone_config(data: [u8; 7]) -> Result<Value, EncodeError> {
    let cfg = ZoneConfig::from(data);
    dto_to_value(&ZoneConfigDto {
        header: cfg.header,
        total_zones: cfg.num_zones,
        constant_zones: cfg.num_constant,
        constant_zone_ids: cfg.constant,
        filter_clean_required: cfg.filter_clean,
    })
}

// --- Reg 02: unit activation (direct codec) ---------------------------------
//
// Wire: [unit_type][activation_status][dict_fw_major][dict_fw_minor][0][0][0].
// `unit_type` byte mapping is not normative in issue #27; see module docs.

pub(super) fn encode_unit_activation(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<UnitTypeEnum>("unit_type", payload)?;
    check_enum::<ActivationStatus>("activation_status", payload)?;
    let dto: UnitActivationDto = deserialize_payload(payload)?;
    Ok([
        unit_type_to_byte(dto.unit_type),
        activation_status_to_byte(dto.activation_status),
        dto.dict_fw_major,
        dto.dict_fw_minor,
        0,
        0,
        0,
    ])
}

pub(super) fn decode_unit_activation(data: [u8; 7]) -> Result<Value, EncodeError> {
    let Some(unit_type) = unit_type_from_byte(data[0]) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    let Some(activation_status) = activation_status_from_byte(data[1]) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    dto_to_value(&UnitActivationDto {
        unit_type,
        activation_status,
        dict_fw_major: data[2],
        dict_fw_minor: data[3],
    })
}

const fn unit_type_to_byte(unit_type: UnitTypeEnum) -> u8 {
    match unit_type {
        UnitTypeEnum::Daikin => 0x00,
        UnitTypeEnum::Panasonic => 0x01,
        UnitTypeEnum::Fujitsu => 0x02,
        UnitTypeEnum::SamsungDvm => 0x03,
    }
}

const fn unit_type_from_byte(byte: u8) -> Option<UnitTypeEnum> {
    match byte {
        0x00 => Some(UnitTypeEnum::Daikin),
        0x01 => Some(UnitTypeEnum::Panasonic),
        0x02 => Some(UnitTypeEnum::Fujitsu),
        0x03 => Some(UnitTypeEnum::SamsungDvm),
        _ => None,
    }
}

const fn activation_status_to_byte(status: ActivationStatus) -> u8 {
    match status {
        ActivationStatus::NoCode => 0x00,
        ActivationStatus::CodeEnabled => 0x01,
        ActivationStatus::Expired => 0x02,
    }
}

const fn activation_status_from_byte(byte: u8) -> Option<ActivationStatus> {
    match byte {
        0x00 => Some(ActivationStatus::NoCode),
        0x01 => Some(ActivationStatus::CodeEnabled),
        0x02 => Some(ActivationStatus::Expired),
        _ => None,
    }
}

// --- Reg 03: zone state (aa-registers `ZoneState`) --------------------------
//
// The zone id is part of the address, not the payload. Internal helpers take
// the zone explicitly; the public entry points stamp `0` on encode and drop
// the zone byte on decode (see module docs).

pub(super) fn encode_zone_state(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<SensorTypeEnum>("sensor_type", payload)?;
    let dto: ZoneStateDto = deserialize_payload(payload)?;
    Ok(zone_state_to_wire(&dto, 0))
}

/// Encode a zone-state DTO with an explicit zone id.
fn zone_state_to_wire(dto: &ZoneStateDto, zone: u8) -> [u8; 7] {
    let (meas_int, meas_dec) = measured_from_c(dto.measured_temp_c);
    ZoneState {
        zone,
        open: dto.open,
        percent: dto.damper_pct,
        sensor: sensor_type_to_aa(dto.sensor_type),
        set_temp_x2: temp_c_to_x2(dto.target_temp_c),
        meas_int,
        meas_dec,
    }
    .into()
}

pub(super) fn decode_zone_state(data: [u8; 7]) -> Result<Value, EncodeError> {
    let state = ZoneState::from(data);
    let Some(sensor_type) = sensor_type_from_aa(state.sensor) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    dto_to_value(&ZoneStateDto {
        open: state.open,
        damper_pct: state.percent,
        sensor_type,
        target_temp_c: temp_x2_to_c(state.set_temp_x2),
        measured_temp_c: measured_to_c(state.meas_int, state.meas_dec),
    })
}

const fn sensor_type_to_aa(sensor: SensorTypeEnum) -> SensorType {
    match sensor {
        SensorTypeEnum::NoSensor => SensorType::NoSensor,
        SensorTypeEnum::Rf => SensorType::Rf,
        SensorTypeEnum::Wired => SensorType::Wired,
        SensorTypeEnum::Rf2CanBooster => SensorType::Rf2CanBooster,
        SensorTypeEnum::RfX => SensorType::RfX,
    }
}

const fn sensor_type_from_aa(sensor: SensorType) -> Option<SensorTypeEnum> {
    match sensor {
        SensorType::NoSensor => Some(SensorTypeEnum::NoSensor),
        SensorType::Rf => Some(SensorTypeEnum::Rf),
        SensorType::Wired => Some(SensorTypeEnum::Wired),
        SensorType::Rf2CanBooster => Some(SensorTypeEnum::Rf2CanBooster),
        SensorType::RfX => Some(SensorTypeEnum::RfX),
        SensorType::Unknown(_) => None,
    }
}

// --- Reg 04: zone limits (direct codec) -------------------------------------
//
// Wire: [zone][min_damper][max_damper][motion_status][motion_config]
//       [zone_error][rssi]. Zone byte `0x00` on encode (same address
//       limitation as reg `03`), dropped on decode.

pub(super) fn encode_zone_limits(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: ZoneLimitsDto = deserialize_payload(payload)?;
    Ok([
        0x00,
        dto.min_damper,
        dto.max_damper,
        dto.motion_status,
        dto.motion_config,
        dto.zone_error,
        dto.rssi,
    ])
}

pub(super) fn decode_zone_limits(data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&ZoneLimitsDto {
        min_damper: data[1],
        max_damper: data[2],
        motion_status: data[3],
        motion_config: data[4],
        zone_error: data[5],
        rssi: data[6],
    })
}

// --- Reg 05: system status (aa-registers `SystemStatus`) --------------------

pub(super) fn encode_system_status(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<PowerEnum>("power", payload)?;
    check_enum::<ModeEnum>("mode", payload)?;
    check_enum::<FanEnum>("fan", payload)?;
    let dto: SystemStatusDto = deserialize_payload(payload)?;
    Ok(SystemStatus {
        power: power_to_aa(dto.power),
        mode: mode_to_aa(dto.mode),
        fan: fan_to_aa(dto.fan),
        set_temp_x2: temp_c_to_x2(dto.target_temp_c),
        myzone_id: dto.myzone_id,
        fresh_air: if dto.fresh_air {
            FreshAir::On
        } else {
            FreshAir::Off
        },
        rf_sys_id: dto.rf_sys_id,
    }
    .into())
}

pub(super) fn decode_system_status(data: [u8; 7]) -> Result<Value, EncodeError> {
    let status = SystemStatus::from(data);
    let Some(power) = power_from_aa(status.power) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    let Some(mode) = mode_from_aa(status.mode) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    let Some(fan) = fan_from_aa(status.fan) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    let fresh_air = match status.fresh_air {
        FreshAir::On => true,
        FreshAir::None | FreshAir::Off => false,
        FreshAir::Unknown(_) => return Ok(Value::String(bytes_to_hex(data))),
    };
    dto_to_value(&SystemStatusDto {
        power,
        mode,
        fan,
        target_temp_c: temp_x2_to_c(status.set_temp_x2),
        myzone_id: status.myzone_id,
        fresh_air,
        rf_sys_id: status.rf_sys_id,
    })
}

const fn power_to_aa(power: PowerEnum) -> Power {
    match power {
        PowerEnum::On => Power::On,
        PowerEnum::Off => Power::Off,
    }
}

const fn power_from_aa(power: Power) -> Option<PowerEnum> {
    match power {
        Power::On => Some(PowerEnum::On),
        Power::Off => Some(PowerEnum::Off),
        Power::Unknown(_) => None,
    }
}

const fn mode_to_aa(mode: ModeEnum) -> Mode {
    match mode {
        ModeEnum::Cool => Mode::Cool,
        ModeEnum::Heat => Mode::Heat,
        ModeEnum::Vent => Mode::Vent,
        ModeEnum::Auto => Mode::Auto,
        ModeEnum::Dry => Mode::Dry,
        ModeEnum::MyAuto => Mode::MyAuto,
    }
}

const fn mode_from_aa(mode: Mode) -> Option<ModeEnum> {
    match mode {
        Mode::Cool => Some(ModeEnum::Cool),
        Mode::Heat => Some(ModeEnum::Heat),
        Mode::Vent => Some(ModeEnum::Vent),
        Mode::Auto => Some(ModeEnum::Auto),
        Mode::Dry => Some(ModeEnum::Dry),
        Mode::MyAuto => Some(ModeEnum::MyAuto),
        Mode::Unknown(_) => None,
    }
}

const fn fan_to_aa(fan: FanEnum) -> Fan {
    match fan {
        FanEnum::Off => Fan::Off,
        FanEnum::Low => Fan::Low,
        FanEnum::Medium => Fan::Medium,
        FanEnum::High => Fan::High,
        FanEnum::Auto => Fan::Auto,
        FanEnum::AutoAa => Fan::AutoAa,
    }
}

const fn fan_from_aa(fan: Fan) -> Option<FanEnum> {
    match fan {
        Fan::Off => Some(FanEnum::Off),
        Fan::Low => Some(FanEnum::Low),
        Fan::Medium => Some(FanEnum::Medium),
        Fan::High => Some(FanEnum::High),
        Fan::Auto => Some(FanEnum::Auto),
        Fan::AutoAa => Some(FanEnum::AutoAa),
        Fan::Unknown(_) => None,
    }
}

// --- Reg 06: firmware (aa-registers `FirmwareStatus`) -----------------------

pub(super) fn encode_firmware(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: FirmwareDto = deserialize_payload(payload)?;
    Ok(FirmwareStatus {
        fw_major: dto.fw_major,
        fw_minor: dto.fw_minor,
        cb_type: dto.cb_type,
        rf_fw_major: dto.rf_fw_major,
    }
    .into())
}

pub(super) fn decode_firmware(data: [u8; 7]) -> Result<Value, EncodeError> {
    let fw = FirmwareStatus::from(data);
    dto_to_value(&FirmwareDto {
        fw_major: fw.fw_major,
        fw_minor: fw.fw_minor,
        cb_type: fw.cb_type,
        rf_fw_major: fw.rf_fw_major,
    })
}

// --- Reg 08: system error (direct codec) ------------------------------------
//
// Wire: [5 ASCII][00][00]. Encode requires exactly 5 ASCII chars; decode reads
// 5 chars and trims trailing NULs/spaces.

pub(super) fn encode_system_error(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: SystemErrorDto = deserialize_payload(payload)?;
    if dto.error_code.len() != 5 || !dto.error_code.is_ascii() {
        return Err(EncodeError::BadPayload(format!(
            "error_code must be exactly 5 ASCII chars: {:?}",
            dto.error_code
        )));
    }
    let mut data = [0u8; 7];
    data[..5].copy_from_slice(dto.error_code.as_bytes());
    Ok(data)
}

pub(super) fn decode_system_error(data: [u8; 7]) -> Result<Value, EncodeError> {
    let mut code = String::with_capacity(5);
    for &byte in &data[..5] {
        code.push(char::from(byte));
    }
    dto_to_value(&SystemErrorDto {
        error_code: code.trim_end_matches(['\0', ' ']).to_owned(),
    })
}

// --- Reg 09: activation code (direct codec) ---------------------------------
//
// Wire: [action][code_hi][code_lo][days][0][0][0]. `unlock_code` is a 4-char
// hex string; `"A1B2"` → bytes `0xA1`, `0xB2`.

pub(super) fn encode_activation_code(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<ActionEnum>("action", payload)?;
    let dto: ActivationCodeDto = deserialize_payload(payload)?;
    if dto.unlock_code.len() != 4 || !dto.unlock_code.is_ascii() {
        return Err(EncodeError::BadHex(dto.unlock_code));
    }
    let mut data = [0u8; 7];
    data[0] = action_to_byte(dto.action);
    data[1] = hex2_to_byte(&dto.unlock_code[..2])?;
    data[2] = hex2_to_byte(&dto.unlock_code[2..])?;
    data[3] = dto.activation_days;
    Ok(data)
}

pub(super) fn decode_activation_code(data: [u8; 7]) -> Result<Value, EncodeError> {
    let Some(action) = action_from_byte(data[0]) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    dto_to_value(&ActivationCodeDto {
        action,
        unlock_code: slice_to_hex(&data[1..3]),
        activation_days: data[3],
    })
}

/// Parse exactly two hex characters into a byte.
///
/// # Errors
///
/// Returns [`EncodeError::BadHex`] if `s` is not exactly two hex characters.
fn hex2_to_byte(s: &str) -> Result<u8, EncodeError> {
    let bytes = s.as_bytes();
    let hi = hex_nibble(bytes[0]).ok_or_else(|| EncodeError::BadHex(s.to_owned()))?;
    let lo = hex_nibble(bytes[1]).ok_or_else(|| EncodeError::BadHex(s.to_owned()))?;
    Ok((hi << 4) | lo)
}

const fn action_to_byte(action: ActionEnum) -> u8 {
    match action {
        ActionEnum::SetCode => 0x00,
        ActionEnum::Unlock => 0x01,
    }
}

const fn action_from_byte(byte: u8) -> Option<ActionEnum> {
    match byte {
        0x00 => Some(ActionEnum::SetCode),
        0x01 => Some(ActionEnum::Unlock),
        _ => None,
    }
}

// --- Reg 0a: unit announcement (direct codec) --------------------------------
//
// Empty payload: encode → all zeros, decode → `{}`.

pub(super) fn encode_unit_announcement(payload: &Value) -> Result<[u8; 7], EncodeError> {
    // Validate the payload is a JSON object (empty DTO — the register carries
    // no fields); the wire bytes are all zero.
    deserialize_payload::<UnitAnnouncementDto>(payload)?;
    Ok([0; 7])
}

pub(super) fn decode_unit_announcement(_data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&UnitAnnouncementDto {})
}

// --- Reg 12: sensor pairing (direct codec) ----------------------------------
//
// Read wire:  [uid 3B][info][rev][0][0]; write wire: [uid 3B][zone][0][0][0].
// Read vs write shape disambiguation is documented in the module docs.

pub(super) fn encode_sensor_pairing(payload: &Value) -> Result<[u8; 7], EncodeError> {
    // Read shape wins when both shapes could match: the read DTO carries
    // `pairing`/`sensor_rev` which the write DTO lacks, so any payload that
    // deserializes as `SensorPairingDto` is treated as a read.
    if let Ok(read) = serde_json::from_value::<SensorPairingDto>(payload.clone()) {
        return encode_sensor_pairing_read(&read);
    }
    let write: SensorPairingWriteDto = deserialize_payload(payload)?;
    encode_sensor_pairing_write(&write)
}

fn encode_sensor_pairing_read(dto: &SensorPairingDto) -> Result<[u8; 7], EncodeError> {
    let mut data = [0u8; 7];
    data[..3].copy_from_slice(&sensor_uid_to_bytes(&dto.sensor_uid)?);
    data[3] = u8::from(dto.pairing);
    data[4] = dto.sensor_rev;
    Ok(data)
}

fn encode_sensor_pairing_write(dto: &SensorPairingWriteDto) -> Result<[u8; 7], EncodeError> {
    let mut data = [0u8; 7];
    data[..3].copy_from_slice(&sensor_uid_to_bytes(&dto.sensor_uid)?);
    data[3] = dto.zone;
    Ok(data)
}

pub(super) fn decode_sensor_pairing(data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&SensorPairingDto {
        sensor_uid: slice_to_hex(&data[..3]),
        pairing: data[3] != 0,
        sensor_rev: data[4],
    })
}

/// Parse a 6-hex-char sensor uid into 3 wire bytes (`"01613d"` → `[0x01, 0x61,
/// 0x3d]`).
///
/// # Errors
///
/// Returns [`EncodeError::BadHex`] if `uid` is not exactly 6 hex characters.
fn sensor_uid_to_bytes(uid: &str) -> Result<[u8; 3], EncodeError> {
    if uid.len() != 6 || !uid.is_ascii() {
        return Err(EncodeError::BadHex(uid.to_owned()));
    }
    let bytes = uid.as_bytes();
    let mut out = [0u8; 3];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[2 * i]).ok_or_else(|| EncodeError::BadHex(uid.to_owned()))?;
        let lo = hex_nibble(bytes[2 * i + 1]).ok_or_else(|| EncodeError::BadHex(uid.to_owned()))?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

// --- Reg 13: info byte (direct codec) ---------------------------------------

pub(super) fn encode_info_byte(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: InfoByteDto = deserialize_payload(payload)?;
    let mut data = [0u8; 7];
    data[0] = dto.info_byte;
    Ok(data)
}

pub(super) fn decode_info_byte(data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&InfoByteDto { info_byte: data[0] })
}

// --- Reg 26: RF device pairing (direct codec) --------------------------------

pub(super) fn encode_rf_device_pairing(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: RfDevicePairingDto = deserialize_payload(payload)?;
    Ok([
        dto.pairing_control,
        dto.rf_device_type,
        dto.zone_channel,
        0,
        0,
        0,
        0,
    ])
}

pub(super) fn decode_rf_device_pairing(data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&RfDevicePairingDto {
        pairing_control: data[0],
        rf_device_type: data[1],
        zone_channel: data[2],
    })
}

// --- Reg 27: RF device calibration (direct codec) ----------------------------
//
// Wire order is channel BEFORE position: [calibration_control][channel]
// [up_down_position][0][0][0][0].

pub(super) fn encode_rf_device_calibration(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: RfDeviceCalibrationDto = deserialize_payload(payload)?;
    Ok([
        dto.calibration_control,
        dto.channel,
        dto.up_down_position,
        0,
        0,
        0,
        0,
    ])
}

pub(super) fn decode_rf_device_calibration(data: [u8; 7]) -> Result<Value, EncodeError> {
    dto_to_value(&RfDeviceCalibrationDto {
        calibration_control: data[0],
        channel: data[1],
        up_down_position: data[2],
    })
}
