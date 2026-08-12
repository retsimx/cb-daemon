//! Typed register DTOs and wire-string enums for the register catalog.

use serde::{Deserialize, Serialize};

/// Ack status string on the wire (`success` | `error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    /// Update / command accepted.
    Success,
    /// Update / command rejected.
    Error,
}

/// HVAC operating mode (reg `05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeEnum {
    /// Cool.
    Cool,
    /// Heat.
    Heat,
    /// Vent.
    Vent,
    /// Auto.
    Auto,
    /// Dry.
    Dry,
    /// `MyAuto` — `myauto` on the wire (`snake_case` would give `my_auto`).
    #[serde(rename = "myauto")]
    MyAuto,
}

/// Fan speed (reg `05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanEnum {
    /// Off.
    Off,
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Auto.
    Auto,
    /// `AutoAA`.
    AutoAa,
}

/// Power / system on-off (reg `05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerEnum {
    /// On.
    On,
    /// Off.
    Off,
}

/// Zone temperature sensor type (reg `03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorTypeEnum {
    /// No sensor.
    NoSensor,
    /// RF sensor.
    Rf,
    /// Wired sensor.
    Wired,
    /// RF2CAN booster — `rf2can_booster` on the wire (`snake_case` would give
    /// `rf2_can_booster`).
    #[serde(rename = "rf2can_booster")]
    Rf2CanBooster,
    /// `RF_X`.
    RfX,
}

/// Unit activation status (reg `02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationStatus {
    /// No activation code present.
    NoCode,
    /// Activation code enabled.
    CodeEnabled,
    /// Activation code expired.
    Expired,
}

/// Unit type (reg `02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitTypeEnum {
    /// Daikin.
    Daikin,
    /// Panasonic.
    Panasonic,
    /// Fujitsu.
    Fujitsu,
    /// Samsung DVM.
    SamsungDvm,
}

/// Activation-code action (reg `09`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionEnum {
    /// Set a new activation code.
    SetCode,
    /// Unlock the unit.
    Unlock,
}

/// Broker link state (`ServerMessage::Status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    /// Negotiating with the unit.
    Negotiating,
    /// Link established, mailbox in sync.
    Synced,
    /// Resynchronising the mailbox.
    Resyncing,
    /// Link down.
    LinkDown,
}

/// Reg `01` zone configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ZoneConfigDto {
    /// Opaque wire header byte.
    pub header: u8,
    /// Number of zones.
    pub total_zones: u8,
    /// Number of constant zones.
    pub constant_zones: u8,
    /// Constant zone ids (unused slots `0`).
    pub constant_zone_ids: [u8; 3],
    /// Filter-clean flag.
    pub filter_clean_required: bool,
}

/// Reg `02` unit activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UnitActivationDto {
    /// Unit type.
    pub unit_type: UnitTypeEnum,
    /// Activation status.
    pub activation_status: ActivationStatus,
    /// Dictionary firmware major.
    pub dict_fw_major: u8,
    /// Dictionary firmware minor.
    pub dict_fw_minor: u8,
}

/// Reg `03` per-zone state (snapshot map value / event body fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ZoneStateDto {
    /// Zone damper open.
    pub open: bool,
    /// Damper percent 0–100.
    pub damper_pct: u8,
    /// Sensor type.
    pub sensor_type: SensorTypeEnum,
    /// Target temperature in °C.
    pub target_temp_c: f64,
    /// Measured temperature in °C.
    pub measured_temp_c: f64,
}

/// Reg `04` per-zone limits and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ZoneLimitsDto {
    /// Minimum damper percent.
    pub min_damper: u8,
    /// Maximum damper percent.
    pub max_damper: u8,
    /// Motion status.
    pub motion_status: u8,
    /// Motion configuration.
    pub motion_config: u8,
    /// Zone error code.
    pub zone_error: u8,
    /// RF signal strength.
    pub rssi: u8,
}

/// Reg `05` system status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SystemStatusDto {
    /// Power state.
    pub power: PowerEnum,
    /// Operating mode.
    pub mode: ModeEnum,
    /// Fan speed.
    pub fan: FanEnum,
    /// Target temperature in °C.
    pub target_temp_c: f64,
    /// `MyZone` id (`0` = disabled / default).
    pub myzone_id: u8,
    /// Fresh-air on/off.
    pub fresh_air: bool,
    /// RF system id.
    pub rf_sys_id: u8,
}

/// Reg `06` firmware versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FirmwareDto {
    /// Firmware major version.
    pub fw_major: u8,
    /// Firmware minor version.
    pub fw_minor: u8,
    /// Control-board type.
    pub cb_type: u8,
    /// RF firmware major version.
    pub rf_fw_major: u8,
}

/// Reg `08` system error code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SystemErrorDto {
    /// ASCII error code (5 chars).
    pub error_code: String,
}

/// Reg `09` activation-code write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ActivationCodeDto {
    /// Action (`set_code` | `unlock`).
    pub action: ActionEnum,
    /// Unlock code (4 hex chars).
    pub unlock_code: String,
    /// Activation days.
    pub activation_days: u8,
}

/// Reg `0a` unit announcement (empty payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitAnnouncementDto {}

/// Reg `12` sensor pairing read shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SensorPairingDto {
    /// Sensor uid (6 hex chars).
    pub sensor_uid: String,
    /// Pairing in progress.
    pub pairing: bool,
    /// Sensor revision.
    pub sensor_rev: u8,
}

/// Reg `12` sensor pairing write shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SensorPairingWriteDto {
    /// Sensor uid (6 hex chars).
    pub sensor_uid: String,
    /// Zone to pair the sensor to.
    pub zone: u8,
}

/// Reg `13` info byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InfoByteDto {
    /// Info byte.
    pub info_byte: u8,
}

/// Reg `26` RF device pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RfDevicePairingDto {
    /// Pairing control code.
    pub pairing_control: u8,
    /// RF device type.
    pub rf_device_type: u8,
    /// Zone channel.
    pub zone_channel: u8,
}

/// Reg `27` RF device calibration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RfDeviceCalibrationDto {
    /// Calibration control code.
    pub calibration_control: u8,
    /// Channel.
    pub channel: u8,
    /// Up/down position.
    pub up_down_position: u8,
}
