//! Register IDs, CAN2 wire codec, and register bank for Advantage Air CB protocol.
//!
//! Layered modules: [`ids`] (typed identifiers), [`wire`] (hex encode/decode),
//! [`bank`] (in-memory mailbox), [`typed`] (decoded register helpers).

pub mod bank;
pub mod ids;
pub mod typed;
pub mod wire;

pub use bank::{BankKey, RegisterBank, ZONE_BEARING_REGS, is_zone_bearing};
pub use ids::{IdError, RegId, UNIT_ID_MAX, UnitId, UnitType};
pub use typed::{
    DecodedRegister, Fan, FirmwareStatus, FreshAir, Mode, Power, SensorType, SystemStatus,
    ZoneConfig, ZoneState,
};
pub use wire::{CanRecord, Dest, RECORD_HEX_LEN, WireError};
