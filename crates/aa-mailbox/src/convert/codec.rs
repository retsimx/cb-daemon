//! Per-register wire codecs (one section per register).
//!
//! Called from the [`super::encode_payload`] / [`super::decode_payload`]
//! dispatch in the parent module, which owns the shared helpers
//! (hex / temperature / DTO (de)serialization). Every register builds/decode
//! through the [`aa_registers`] typed structs; register-specific helpers
//! (`zone_state_to_wire`, `sensor_uid_to_bytes`, `hex2_to_byte`, the DTO↔
//! aa-registers enum mappers) stay here next to their register.

use aa_registers::{
    Action, ActivationCode, ActivationStatus as AaActivationStatus, Fan, FirmwareStatus, FreshAir,
    InfoByte, Mode, Power, RfDeviceCalibration, RfDevicePairing, SensorPairingRead,
    SensorPairingWrite, SensorType, SystemError, SystemStatus, UnitActivation, UnitAnnouncement,
    UnitBrand, ZoneConfig, ZoneLimits, ZoneState,
};
use serde_json::Value;

use super::{
    bytes_to_hex, check_enum, deserialize_payload, dto_to_value, hex_nibble, measured_from_c,
    measured_to_c, slice_to_hex, temp_c_to_x2, temp_x2_to_c,
};
use crate::dto::{
    ActionEnum, ActivationCodeDto, ActivationStatus, FanEnum, FirmwareDto, FreshAirEnum,
    InfoByteDto, ModeEnum, PowerEnum, RfDeviceCalibrationDto, RfDevicePairingDto, SensorPairingDto,
    SensorPairingWriteDto, SensorTypeEnum, SystemErrorDto, SystemStatusDto, UnitActivationDto,
    UnitAnnouncementDto, UnitTypeEnum, ZoneConfigDto, ZoneLimitsDto, ZoneStateDto,
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

// --- Reg 02: unit activation (aa-registers `UnitActivation`) ----------------
//
// Wire: [unit_type][activation_status][dict_fw_major][dict_fw_minor][0][0][0].
// `unit_type` wire values are normative from aa_interop (`0x11`/`0x12`/`0x13`/
// `0x19`, via the `UnitBrand` enum); see module docs.

pub(super) fn encode_unit_activation(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<UnitTypeEnum>("unit_type", payload)?;
    check_enum::<ActivationStatus>("activation_status", payload)?;
    let dto: UnitActivationDto = deserialize_payload(payload)?;
    Ok(UnitActivation {
        unit_type: unit_brand_to_aa(dto.unit_type),
        activation_status: activation_status_to_aa(dto.activation_status),
        dict_fw_major: dto.dict_fw_major,
        dict_fw_minor: dto.dict_fw_minor,
    }
    .into())
}

pub(super) fn decode_unit_activation(data: [u8; 7]) -> Result<Value, EncodeError> {
    let activation = UnitActivation::from(data);
    let Some(unit_type) = unit_brand_from_aa(activation.unit_type) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    let Some(activation_status) = activation_status_from_aa(activation.activation_status) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    dto_to_value(&UnitActivationDto {
        unit_type,
        activation_status,
        dict_fw_major: activation.dict_fw_major,
        dict_fw_minor: activation.dict_fw_minor,
    })
}

const fn unit_brand_to_aa(unit_type: UnitTypeEnum) -> UnitBrand {
    match unit_type {
        UnitTypeEnum::Daikin => UnitBrand::Daikin,
        UnitTypeEnum::Panasonic => UnitBrand::Panasonic,
        UnitTypeEnum::Fujitsu => UnitBrand::Fujitsu,
        UnitTypeEnum::SamsungDvm => UnitBrand::SamsungDvm,
    }
}

const fn unit_brand_from_aa(unit_type: UnitBrand) -> Option<UnitTypeEnum> {
    match unit_type {
        UnitBrand::Daikin => Some(UnitTypeEnum::Daikin),
        UnitBrand::Panasonic => Some(UnitTypeEnum::Panasonic),
        UnitBrand::Fujitsu => Some(UnitTypeEnum::Fujitsu),
        UnitBrand::SamsungDvm => Some(UnitTypeEnum::SamsungDvm),
        UnitBrand::Unknown(_) => None,
    }
}

const fn activation_status_to_aa(status: ActivationStatus) -> AaActivationStatus {
    match status {
        ActivationStatus::NoCode => AaActivationStatus::NoCode,
        ActivationStatus::CodeEnabled => AaActivationStatus::CodeEnabled,
        ActivationStatus::Expired => AaActivationStatus::Expired,
    }
}

const fn activation_status_from_aa(status: AaActivationStatus) -> Option<ActivationStatus> {
    match status {
        AaActivationStatus::NoCode => Some(ActivationStatus::NoCode),
        AaActivationStatus::CodeEnabled => Some(ActivationStatus::CodeEnabled),
        AaActivationStatus::Expired => Some(ActivationStatus::Expired),
        AaActivationStatus::Unknown(_) => None,
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

// --- Reg 04: zone limits (aa-registers `ZoneLimits`) ------------------------
//
// Wire: [zone][min_damper][max_damper][motion_status][motion_config]
//       [zone_error][rssi]. Zone byte `0x00` on encode (same address
//       limitation as reg `03`), dropped on decode.

pub(super) fn encode_zone_limits(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: ZoneLimitsDto = deserialize_payload(payload)?;
    Ok(ZoneLimits {
        zone: 0x00,
        min_damper: dto.min_damper,
        max_damper: dto.max_damper,
        motion_status: dto.motion_status,
        motion_config: dto.motion_config,
        zone_error: dto.zone_error,
        rssi: dto.rssi,
    }
    .into())
}

pub(super) fn decode_zone_limits(data: [u8; 7]) -> Result<Value, EncodeError> {
    let limits = ZoneLimits::from(data);
    dto_to_value(&ZoneLimitsDto {
        min_damper: limits.min_damper,
        max_damper: limits.max_damper,
        motion_status: limits.motion_status,
        motion_config: limits.motion_config,
        zone_error: limits.zone_error,
        rssi: limits.rssi,
    })
}

// --- Reg 05: system status (aa-registers `SystemStatus`) --------------------

pub(super) fn encode_system_status(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<PowerEnum>("power", payload)?;
    check_enum::<ModeEnum>("mode", payload)?;
    check_enum::<FanEnum>("fan", payload)?;
    check_enum::<FreshAirEnum>("fresh_air", payload)?;
    let dto: SystemStatusDto = deserialize_payload(payload)?;
    Ok(SystemStatus {
        power: power_to_aa(dto.power),
        mode: mode_to_aa(dto.mode),
        fan: fan_to_aa(dto.fan),
        set_temp_x2: temp_c_to_x2(dto.target_temp_c),
        myzone_id: dto.myzone_id,
        fresh_air: fresh_air_to_aa(dto.fresh_air),
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
        FreshAir::On => FreshAirEnum::On,
        FreshAir::Off => FreshAirEnum::Off,
        FreshAir::None => FreshAirEnum::None,
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

const fn fresh_air_to_aa(fresh_air: FreshAirEnum) -> FreshAir {
    match fresh_air {
        FreshAirEnum::None => FreshAir::None,
        FreshAirEnum::Off => FreshAir::Off,
        FreshAirEnum::On => FreshAir::On,
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

// --- Reg 08: system error (aa-registers `SystemError`) ----------------------
//
// Wire: [5 ASCII][00][00]. The typed struct keeps the raw 5 bytes untrimmed;
// the codec enforces exactly 5 ASCII chars on encode and trims trailing
// NULs/spaces for the DTO on decode.

pub(super) fn encode_system_error(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: SystemErrorDto = deserialize_payload(payload)?;
    if dto.error_code.len() != 5 || !dto.error_code.is_ascii() {
        return Err(EncodeError::BadPayload(format!(
            "error_code must be exactly 5 ASCII chars: {:?}",
            dto.error_code
        )));
    }
    let mut raw = [0u8; 5];
    raw.copy_from_slice(dto.error_code.as_bytes());
    Ok(SystemError { error_code: raw }.into())
}

pub(super) fn decode_system_error(data: [u8; 7]) -> Result<Value, EncodeError> {
    let error = SystemError::from(data);
    let mut code = String::with_capacity(5);
    for &byte in &error.error_code {
        code.push(char::from(byte));
    }
    dto_to_value(&SystemErrorDto {
        error_code: code.trim_end_matches(['\0', ' ']).to_owned(),
    })
}

// --- Reg 09: activation code (aa-registers `ActivationCode`) ----------------
//
// Wire: [action][code_hi][code_lo][days][0][0][0]. `unlock_code` is a 4-char
// hex string; `"A1B2"` → bytes `0xA1`, `0xB2`. `action` wire values are
// normative from aa_interop (`1` = set code, `2` = unlock, via the `Action`
// enum).

pub(super) fn encode_activation_code(payload: &Value) -> Result<[u8; 7], EncodeError> {
    check_enum::<ActionEnum>("action", payload)?;
    let dto: ActivationCodeDto = deserialize_payload(payload)?;
    if dto.unlock_code.len() != 4 || !dto.unlock_code.is_ascii() {
        return Err(EncodeError::BadHex(dto.unlock_code));
    }
    Ok(ActivationCode {
        action: action_to_aa(dto.action),
        unlock_code: [
            hex2_to_byte(&dto.unlock_code[..2])?,
            hex2_to_byte(&dto.unlock_code[2..])?,
        ],
        activation_days: dto.activation_days,
    }
    .into())
}

pub(super) fn decode_activation_code(data: [u8; 7]) -> Result<Value, EncodeError> {
    let code = ActivationCode::from(data);
    let Some(action) = action_from_aa(code.action) else {
        return Ok(Value::String(bytes_to_hex(data)));
    };
    dto_to_value(&ActivationCodeDto {
        action,
        unlock_code: slice_to_hex(&code.unlock_code),
        activation_days: code.activation_days,
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

const fn action_to_aa(action: ActionEnum) -> Action {
    match action {
        ActionEnum::SetCode => Action::SetCode,
        ActionEnum::Unlock => Action::Unlock,
    }
}

const fn action_from_aa(action: Action) -> Option<ActionEnum> {
    match action {
        Action::SetCode => Some(ActionEnum::SetCode),
        Action::Unlock => Some(ActionEnum::Unlock),
        Action::Unknown(_) => None,
    }
}

// --- Reg 0a: unit announcement (aa-registers `UnitAnnouncement`) -------------
//
// Empty payload: encode → all zeros, decode → `{}`.

pub(super) fn encode_unit_announcement(payload: &Value) -> Result<[u8; 7], EncodeError> {
    // Validate the payload is a JSON object (empty DTO — the register carries
    // no fields); the wire bytes are all zero.
    deserialize_payload::<UnitAnnouncementDto>(payload)?;
    Ok(UnitAnnouncement {}.into())
}

pub(super) fn decode_unit_announcement(data: [u8; 7]) -> Result<Value, EncodeError> {
    let _announcement = UnitAnnouncement::from(data);
    dto_to_value(&UnitAnnouncementDto {})
}

// --- Reg 12: sensor pairing (aa-registers `SensorPairingRead`/`Write`) ------
//
// Read wire:  [uid 3B][info][rev][0][0]; write wire: [uid 3B][zone][0][0][0].
// Pairing is info byte bit 6 (`0x40`) on the read shape. Read vs write shape
// disambiguation is documented in the module docs.

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
    Ok(SensorPairingRead {
        sensor_uid: sensor_uid_to_bytes(&dto.sensor_uid)?,
        info_byte: if dto.pairing { 0x40 } else { 0x00 },
        sensor_rev: dto.sensor_rev,
    }
    .into())
}

fn encode_sensor_pairing_write(dto: &SensorPairingWriteDto) -> Result<[u8; 7], EncodeError> {
    Ok(SensorPairingWrite {
        sensor_uid: sensor_uid_to_bytes(&dto.sensor_uid)?,
        zone: dto.zone,
    }
    .into())
}

pub(super) fn decode_sensor_pairing(data: [u8; 7]) -> Result<Value, EncodeError> {
    let read = SensorPairingRead::from(data);
    dto_to_value(&SensorPairingDto {
        sensor_uid: slice_to_hex(&read.sensor_uid),
        pairing: (read.info_byte & 0x40) != 0,
        sensor_rev: read.sensor_rev,
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

// --- Reg 13: info byte (aa-registers `InfoByte`) ----------------------------
//
// The DTO exposes only `info_byte`; the typed struct preserves the 6 trailing
// bytes byte-exact (encode stamps them `0x00` — the DTO carries no values for
// them).

pub(super) fn encode_info_byte(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: InfoByteDto = deserialize_payload(payload)?;
    Ok(InfoByte {
        info_byte: dto.info_byte,
        rest: [0; 6],
    }
    .into())
}

pub(super) fn decode_info_byte(data: [u8; 7]) -> Result<Value, EncodeError> {
    let info = InfoByte::from(data);
    dto_to_value(&InfoByteDto {
        info_byte: info.info_byte,
    })
}

// --- Reg 26: RF device pairing (aa-registers `RfDevicePairing`) --------------

pub(super) fn encode_rf_device_pairing(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: RfDevicePairingDto = deserialize_payload(payload)?;
    Ok(RfDevicePairing {
        pairing_control: dto.pairing_control,
        rf_device_type: dto.rf_device_type,
        zone_channel: dto.zone_channel,
    }
    .into())
}

pub(super) fn decode_rf_device_pairing(data: [u8; 7]) -> Result<Value, EncodeError> {
    let pairing = RfDevicePairing::from(data);
    dto_to_value(&RfDevicePairingDto {
        pairing_control: pairing.pairing_control,
        rf_device_type: pairing.rf_device_type,
        zone_channel: pairing.zone_channel,
    })
}

// --- Reg 27: RF device calibration (aa-registers `RfDeviceCalibration`) ------
//
// Wire order is channel BEFORE position: [calibration_control][channel]
// [up_down_position][0][0][0][0].

pub(super) fn encode_rf_device_calibration(payload: &Value) -> Result<[u8; 7], EncodeError> {
    let dto: RfDeviceCalibrationDto = deserialize_payload(payload)?;
    Ok(RfDeviceCalibration {
        calibration_control: dto.calibration_control,
        channel: dto.channel,
        up_down_position: dto.up_down_position,
    }
    .into())
}

pub(super) fn decode_rf_device_calibration(data: [u8; 7]) -> Result<Value, EncodeError> {
    let calibration = RfDeviceCalibration::from(data);
    dto_to_value(&RfDeviceCalibrationDto {
        calibration_control: calibration.calibration_control,
        channel: calibration.channel,
        up_down_position: calibration.up_down_position,
    })
}
