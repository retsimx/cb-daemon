//! Register JSON DTOs aligned with aaservice northbound fixtures.

use std::collections::BTreeMap;

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

/// Reg `05` system status as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStatusDto {
    /// Power: `off` | `on` (or `unknown_XX` on read).
    pub power: String,
    /// Mode: `cool`, `heat`, `vent`, `auto`, `dry`, `my_auto`, …
    pub mode: String,
    /// Fan: `off`, `low`, `medium`, `high`, `auto`, `auto_aa`, …
    pub fan: String,
    /// Target temperature in °C.
    pub target_temp_c: f64,
    /// `MyZone` id (`0` = disabled / default).
    pub myzone_id: u8,
    /// Fresh-air on/off (wire `On` → `true`, anything else → `false` on read).
    pub fresh_air: bool,
}

/// Reg `01` zone configuration as JSON (opaque wire header omitted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneConfigDto {
    /// Number of zones (`num_zones` on the wire).
    pub total_zones: u8,
    /// Number of constant zones (`num_constant` on the wire).
    pub constant_zones: u8,
    /// Filter-clean flag (`filter_clean` on the wire).
    pub filter_clean_required: bool,
}

/// Per-zone state as JSON (snapshot map value / event body fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneDto {
    /// Zone damper open.
    pub open: bool,
    /// Damper percent 0–100.
    pub damper_pct: u8,
    /// Sensor type string (`wired`, `rf`, `temp` alias on write, …).
    pub sensor_type: String,
    /// Target temperature in °C.
    pub target_temp_c: f64,
    /// Measured temperature in °C.
    pub measured_temp_c: f64,
}

/// Body fields of a `mailbox_snapshot` message (without the `type` tag).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotBody {
    /// Lowercase hex unit id without `0x` (e.g. `abcde`).
    pub unit_id: String,
    /// System status when present in the bank.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_status: Option<SystemStatusDto>,
    /// Zone config when present in the bank.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_config: Option<ZoneConfigDto>,
    /// Zones keyed by decimal zone id string (`"1"`…`"10"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zones: Option<BTreeMap<String, ZoneDto>>,
}
