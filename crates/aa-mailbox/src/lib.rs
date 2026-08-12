//! Northbound protocol message types and typed register DTOs.
//!
//! This crate owns serde message enums and the typed DTOs for every register in
//! the catalog. It does **not** bind TCP/WS.
//!
//! # Message shapes
//!
//! Server → client examples:
//!
//! ```json
//! {"type":"snapshot","units":{"07:181f3":{}}}
//! ```
//!
//! ```json
//! {"type":"event","unit_type":"07","unit_id":"181f3","register":"05","payload":{...}}
//! ```
//!
//! Client → server examples:
//!
//! ```json
//! {"type":"write","msg_id":"1","register":"05","payload":{...}}
//! ```
//!
//! ```json
//! {"type":"command","msg_id":"1","action":"resync"}
//! ```

pub mod convert;
pub mod dto;
pub mod error;
pub mod message;
pub mod policy;

pub use convert::{decode_payload, encode_payload, event_body, snapshot_units, validate_write};
pub use dto::{
    AckStatus, ActionEnum, ActivationStatus, FanEnum, FirmwareDto, InfoByteDto, ModeEnum,
    PowerEnum, RfDeviceCalibrationDto, RfDevicePairingDto, SensorPairingDto, SensorPairingWriteDto,
    SensorTypeEnum, StatusState, SystemErrorDto, SystemStatusDto, UnitActivationDto,
    UnitAnnouncementDto, UnitTypeEnum, ZoneConfigDto, ZoneLimitsDto, ZoneStateDto,
};
pub use error::EncodeError;
pub use message::{ClientMessage, ServerMessage};
pub use policy::{PolicyMode, WritePolicy, write_policy};
