//! Typed register payload structs (`01` / `02` / `03` / `04` / `05` / `06` /
//! `08` / `09` / `0a` / `12` / `13` / `26` / `27`).

use super::enums::{Action, ActivationStatus, UnitBrand};
use super::{Fan, FreshAir, Mode, Power, SensorType};

/// Reg `01` zone configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZoneConfig {
    /// Opaque header byte (`0x11` tablet→CB, `0x20` CB→tablet, etc.).
    pub header: u8,
    /// Number of zones.
    pub num_zones: u8,
    /// Number of constant zones (0–3).
    pub num_constant: u8,
    /// Constant zone ids (unused slots typically `0`).
    pub constant: [u8; 3],
    /// Filter clean status (`true` when wire byte is non-zero).
    pub filter_clean: bool,
}

impl From<ZoneConfig> for [u8; 7] {
    fn from(value: ZoneConfig) -> Self {
        [
            value.header,
            value.num_zones,
            value.num_constant,
            value.constant[0],
            value.constant[1],
            value.constant[2],
            u8::from(value.filter_clean),
        ]
    }
}

impl From<[u8; 7]> for ZoneConfig {
    fn from(data: [u8; 7]) -> Self {
        Self {
            header: data[0],
            num_zones: data[1],
            num_constant: data[2],
            constant: [data[3], data[4], data[5]],
            filter_clean: data[6] != 0,
        }
    }
}

/// Reg `03` per-zone state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZoneState {
    /// Zone number (`0x01`–`0x0a`).
    pub zone: u8,
    /// Zone open (bit 7 of wire byte 1).
    pub open: bool,
    /// Damper percent 0–100 (bits 6–0 of wire byte 1).
    pub percent: u8,
    /// Attached sensor type.
    pub sensor: SensorType,
    /// Set temperature × 2 (wire stores degC × 2).
    pub set_temp_x2: u8,
    /// Measured temperature integer portion.
    pub meas_int: u8,
    /// Measured temperature decimal portion (0–9).
    pub meas_dec: u8,
}

impl From<ZoneState> for [u8; 7] {
    fn from(value: ZoneState) -> Self {
        let open_pct = (u8::from(value.open) << 7) | (value.percent & 0x7f);
        [
            value.zone,
            open_pct,
            value.sensor.to_u8(),
            value.set_temp_x2,
            value.meas_int,
            value.meas_dec,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for ZoneState {
    fn from(data: [u8; 7]) -> Self {
        Self {
            zone: data[0],
            open: (data[1] & 0x80) != 0,
            percent: data[1] & 0x7f,
            sensor: SensorType::from_u8(data[2]),
            set_temp_x2: data[3],
            meas_int: data[4],
            meas_dec: data[5],
        }
    }
}

/// Reg `05` system status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemStatus {
    /// System power.
    pub power: Power,
    /// Operating mode.
    pub mode: Mode,
    /// Fan speed.
    pub fan: Fan,
    /// Set temperature × 2 (wire stores degC × 2).
    pub set_temp_x2: u8,
    /// `MyZone` id (`0` = disabled / default).
    pub myzone_id: u8,
    /// Fresh-air status (README `00`/`01`/`02`).
    pub fresh_air: FreshAir,
    /// RF system id.
    pub rf_sys_id: u8,
}

impl From<SystemStatus> for [u8; 7] {
    fn from(value: SystemStatus) -> Self {
        [
            value.power.to_u8(),
            value.mode.to_u8(),
            value.fan.to_u8(),
            value.set_temp_x2,
            value.myzone_id,
            value.fresh_air.to_u8(),
            value.rf_sys_id,
        ]
    }
}

impl From<[u8; 7]> for SystemStatus {
    fn from(data: [u8; 7]) -> Self {
        Self {
            power: Power::from_u8(data[0]),
            mode: Mode::from_u8(data[1]),
            fan: Fan::from_u8(data[2]),
            set_temp_x2: data[3],
            myzone_id: data[4],
            fresh_air: FreshAir::from_u8(data[5]),
            rf_sys_id: data[6],
        }
    }
}

/// Reg `06` CB firmware / status mailbox payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FirmwareStatus {
    /// CB firmware major.
    pub fw_major: u8,
    /// CB firmware minor.
    pub fw_minor: u8,
    /// Control-box type.
    pub cb_type: u8,
    /// RF firmware major.
    pub rf_fw_major: u8,
}

impl From<FirmwareStatus> for [u8; 7] {
    fn from(value: FirmwareStatus) -> Self {
        [
            value.fw_major,
            value.fw_minor,
            value.cb_type,
            value.rf_fw_major,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for FirmwareStatus {
    fn from(data: [u8; 7]) -> Self {
        Self {
            fw_major: data[0],
            fw_minor: data[1],
            cb_type: data[2],
            rf_fw_major: data[3],
        }
    }
}

/// Reg `02` unit activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnitActivation {
    /// Aircon unit brand.
    pub unit_type: UnitBrand,
    /// Activation-code status.
    pub activation_status: ActivationStatus,
    /// Dictionary firmware major.
    pub dict_fw_major: u8,
    /// Dictionary firmware minor.
    pub dict_fw_minor: u8,
}

impl From<UnitActivation> for [u8; 7] {
    fn from(value: UnitActivation) -> Self {
        [
            value.unit_type.to_u8(),
            value.activation_status.to_u8(),
            value.dict_fw_major,
            value.dict_fw_minor,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for UnitActivation {
    fn from(data: [u8; 7]) -> Self {
        Self {
            unit_type: UnitBrand::from_u8(data[0]),
            activation_status: ActivationStatus::from_u8(data[1]),
            dict_fw_major: data[2],
            dict_fw_minor: data[3],
        }
    }
}

/// Reg `04` per-zone damper limits and status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZoneLimits {
    /// Zone number.
    pub zone: u8,
    /// Minimum damper percent 0–100.
    pub min_damper: u8,
    /// Maximum damper percent 0–100.
    pub max_damper: u8,
    /// Motion current state.
    pub motion_status: u8,
    /// Motion configuration (0–2).
    pub motion_config: u8,
    /// Zone error bit flags.
    pub zone_error: u8,
    /// RF signal strength.
    pub rssi: u8,
}

impl From<ZoneLimits> for [u8; 7] {
    fn from(value: ZoneLimits) -> Self {
        [
            value.zone,
            value.min_damper,
            value.max_damper,
            value.motion_status,
            value.motion_config,
            value.zone_error,
            value.rssi,
        ]
    }
}

impl From<[u8; 7]> for ZoneLimits {
    fn from(data: [u8; 7]) -> Self {
        Self {
            zone: data[0],
            min_damper: data[1],
            max_damper: data[2],
            motion_status: data[3],
            motion_config: data[4],
            zone_error: data[5],
            rssi: data[6],
        }
    }
}

/// Reg `08` system error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemError {
    /// Raw ASCII error code (no trimming; e.g. `AA1` NUL-padded).
    pub error_code: [u8; 5],
}

impl From<SystemError> for [u8; 7] {
    fn from(value: SystemError) -> Self {
        [
            value.error_code[0],
            value.error_code[1],
            value.error_code[2],
            value.error_code[3],
            value.error_code[4],
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for SystemError {
    fn from(data: [u8; 7]) -> Self {
        Self {
            error_code: [data[0], data[1], data[2], data[3], data[4]],
        }
    }
}

/// Reg `09` activation-code command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivationCode {
    /// Requested action.
    pub action: Action,
    /// Unlock code (hi byte first).
    pub unlock_code: [u8; 2],
    /// Activation duration in days.
    pub activation_days: u8,
}

impl From<ActivationCode> for [u8; 7] {
    fn from(value: ActivationCode) -> Self {
        [
            value.action.to_u8(),
            value.unlock_code[0],
            value.unlock_code[1],
            value.activation_days,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for ActivationCode {
    fn from(data: [u8; 7]) -> Self {
        Self {
            action: Action::from_u8(data[0]),
            unlock_code: [data[1], data[2]],
            activation_days: data[3],
        }
    }
}

/// Reg `0a` unit announcement (empty payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnitAnnouncement {}

impl From<UnitAnnouncement> for [u8; 7] {
    fn from(_value: UnitAnnouncement) -> Self {
        [0x00; 7]
    }
}

impl From<[u8; 7]> for UnitAnnouncement {
    fn from(_data: [u8; 7]) -> Self {
        Self {}
    }
}

/// Reg `12` read: RF sensor pairing status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SensorPairingRead {
    /// Sensor unique id.
    pub sensor_uid: [u8; 3],
    /// Raw info byte (bit 6 = pairing requested).
    pub info_byte: u8,
    /// Sensor revision.
    pub sensor_rev: u8,
}

impl From<SensorPairingRead> for [u8; 7] {
    fn from(value: SensorPairingRead) -> Self {
        [
            value.sensor_uid[0],
            value.sensor_uid[1],
            value.sensor_uid[2],
            value.info_byte,
            value.sensor_rev,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for SensorPairingRead {
    fn from(data: [u8; 7]) -> Self {
        Self {
            sensor_uid: [data[0], data[1], data[2]],
            info_byte: data[3],
            sensor_rev: data[4],
        }
    }
}

/// Reg `12` write: RF sensor pairing command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SensorPairingWrite {
    /// Sensor unique id.
    pub sensor_uid: [u8; 3],
    /// Zone number to assign.
    pub zone: u8,
}

impl From<SensorPairingWrite> for [u8; 7] {
    fn from(value: SensorPairingWrite) -> Self {
        [
            value.sensor_uid[0],
            value.sensor_uid[1],
            value.sensor_uid[2],
            value.zone,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for SensorPairingWrite {
    fn from(data: [u8; 7]) -> Self {
        Self {
            sensor_uid: [data[0], data[1], data[2]],
            zone: data[3],
        }
    }
}

/// Reg `13` info byte payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InfoByte {
    /// Raw info byte.
    pub info_byte: u8,
    /// Remaining bytes, preserved byte-exact.
    pub rest: [u8; 6],
}

impl From<InfoByte> for [u8; 7] {
    fn from(value: InfoByte) -> Self {
        [
            value.info_byte,
            value.rest[0],
            value.rest[1],
            value.rest[2],
            value.rest[3],
            value.rest[4],
            value.rest[5],
        ]
    }
}

impl From<[u8; 7]> for InfoByte {
    fn from(data: [u8; 7]) -> Self {
        Self {
            info_byte: data[0],
            rest: [data[1], data[2], data[3], data[4], data[5], data[6]],
        }
    }
}

/// Reg `26` RF device pairing command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RfDevicePairing {
    /// Pairing control byte.
    pub pairing_control: u8,
    /// RF device type (129/130 report as unit type `07`).
    pub rf_device_type: u8,
    /// Zone channel.
    pub zone_channel: u8,
}

impl From<RfDevicePairing> for [u8; 7] {
    fn from(value: RfDevicePairing) -> Self {
        [
            value.pairing_control,
            value.rf_device_type,
            value.zone_channel,
            0x00,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for RfDevicePairing {
    fn from(data: [u8; 7]) -> Self {
        Self {
            pairing_control: data[0],
            rf_device_type: data[1],
            zone_channel: data[2],
        }
    }
}

/// Reg `27` RF device calibration command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RfDeviceCalibration {
    /// Calibration control byte.
    pub calibration_control: u8,
    /// Channel.
    pub channel: u8,
    /// Up/down position.
    pub up_down_position: u8,
}

impl From<RfDeviceCalibration> for [u8; 7] {
    fn from(value: RfDeviceCalibration) -> Self {
        [
            value.calibration_control,
            value.channel,
            value.up_down_position,
            0x00,
            0x00,
            0x00,
            0x00,
        ]
    }
}

impl From<[u8; 7]> for RfDeviceCalibration {
    fn from(data: [u8; 7]) -> Self {
        Self {
            calibration_control: data[0],
            channel: data[1],
            up_down_position: data[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_activation_round_trip() {
        let original = UnitActivation {
            unit_type: UnitBrand::Daikin,
            activation_status: ActivationStatus::CodeEnabled,
            dict_fw_major: 0x02,
            dict_fw_minor: 0x03,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x11, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00]);
        assert_eq!(UnitActivation::from(bytes), original);
    }

    #[test]
    fn zone_limits_round_trip() {
        let original = ZoneLimits {
            zone: 0x01,
            min_damper: 20,
            max_damper: 80,
            motion_status: 2,
            motion_config: 1,
            zone_error: 0x03,
            rssi: 0x2a,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x01, 20, 80, 2, 1, 0x03, 0x2a]);
        assert_eq!(ZoneLimits::from(bytes), original);
    }

    #[test]
    fn system_error_raw_bytes_no_trim() {
        let original = SystemError {
            error_code: [b'A', b'A', b'1', 0x00, b'X'],
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [b'A', b'A', b'1', 0x00, b'X', 0x00, 0x00]);
        let decoded = SystemError::from(bytes);
        assert_eq!(decoded.error_code, [b'A', b'A', b'1', 0x00, b'X']);
        assert_eq!(decoded, original);
    }

    #[test]
    fn activation_code_round_trip() {
        let original = ActivationCode {
            action: Action::Unlock,
            unlock_code: [0xab, 0xcd],
            activation_days: 30,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x02, 0xab, 0xcd, 30, 0x00, 0x00, 0x00]);
        assert_eq!(ActivationCode::from(bytes), original);
    }

    #[test]
    fn unit_announcement_round_trip() {
        let original = UnitAnnouncement {};
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0; 7]);
        assert_eq!(UnitAnnouncement::from(bytes), original);
    }

    #[test]
    fn sensor_pairing_read_round_trip() {
        let original = SensorPairingRead {
            sensor_uid: [0xaa, 0xbb, 0xcc],
            info_byte: 0x40,
            sensor_rev: 0x03,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0xaa, 0xbb, 0xcc, 0x40, 0x03, 0x00, 0x00]);
        assert_eq!(SensorPairingRead::from(bytes), original);
    }

    #[test]
    fn sensor_pairing_write_round_trip() {
        let original = SensorPairingWrite {
            sensor_uid: [0xaa, 0xbb, 0xcc],
            zone: 0x05,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0xaa, 0xbb, 0xcc, 0x05, 0x00, 0x00, 0x00]);
        assert_eq!(SensorPairingWrite::from(bytes), original);
    }

    #[test]
    fn info_byte_preserves_trailing_bytes() {
        let original = InfoByte {
            info_byte: 0x7f,
            rest: [0x01, 0x02, 0x03, 0x04, 0x05, 0x06],
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x7f, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(InfoByte::from(bytes), original);
    }

    #[test]
    fn rf_device_pairing_round_trip() {
        let original = RfDevicePairing {
            pairing_control: 0x01,
            rf_device_type: 129,
            zone_channel: 0x03,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x01, 129, 0x03, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(RfDevicePairing::from(bytes), original);
    }

    #[test]
    fn rf_device_calibration_wire_order() {
        let original = RfDeviceCalibration {
            calibration_control: 0x02,
            channel: 0x0a,
            up_down_position: 0x05,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x02, 0x0a, 0x05, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(RfDeviceCalibration::from(bytes), original);
    }

    #[test]
    fn unit_brand_boundaries() {
        assert_eq!(UnitBrand::from_u8(0x11), UnitBrand::Daikin);
        assert_eq!(UnitBrand::from_u8(0x12), UnitBrand::Panasonic);
        assert_eq!(UnitBrand::from_u8(0x13), UnitBrand::Fujitsu);
        assert_eq!(UnitBrand::from_u8(0x19), UnitBrand::SamsungDvm);
        assert_eq!(UnitBrand::Daikin.to_u8(), 0x11);
        assert_eq!(UnitBrand::Panasonic.to_u8(), 0x12);
        assert_eq!(UnitBrand::Fujitsu.to_u8(), 0x13);
        assert_eq!(UnitBrand::SamsungDvm.to_u8(), 0x19);
        assert_eq!(UnitBrand::from_u8(0x7f), UnitBrand::Unknown(0x7f));
        assert_eq!(UnitBrand::Unknown(0x7f).to_u8(), 0x7f);
    }

    #[test]
    fn activation_status_boundaries() {
        assert_eq!(ActivationStatus::from_u8(0x00), ActivationStatus::NoCode);
        assert_eq!(
            ActivationStatus::from_u8(0x01),
            ActivationStatus::CodeEnabled
        );
        assert_eq!(ActivationStatus::from_u8(0x02), ActivationStatus::Expired);
        assert_eq!(ActivationStatus::NoCode.to_u8(), 0x00);
        assert_eq!(ActivationStatus::CodeEnabled.to_u8(), 0x01);
        assert_eq!(ActivationStatus::Expired.to_u8(), 0x02);
        assert_eq!(
            ActivationStatus::from_u8(0x7f),
            ActivationStatus::Unknown(0x7f)
        );
        assert_eq!(ActivationStatus::Unknown(0x7f).to_u8(), 0x7f);
    }

    #[test]
    fn action_boundaries() {
        assert_eq!(Action::from_u8(0x01), Action::SetCode);
        assert_eq!(Action::from_u8(0x02), Action::Unlock);
        assert_eq!(Action::SetCode.to_u8(), 0x01);
        assert_eq!(Action::Unlock.to_u8(), 0x02);
        assert_eq!(Action::from_u8(0x7f), Action::Unknown(0x7f));
        assert_eq!(Action::Unknown(0x7f).to_u8(), 0x7f);
    }
}
