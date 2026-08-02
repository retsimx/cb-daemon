//! Typed register payload structs (`01` / `03` / `05` / `06`).

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
