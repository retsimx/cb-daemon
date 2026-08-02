//! Northbound mailbox JSON schemas and `RegisterBank` ↔ JSON converters.
//!
//! This crate owns serde message enums and pure conversion helpers used by the
//! WebSocket adapter layer. It does **not** bind TCP/WS.
//!
//! # Message shapes
//!
//! Server → client examples:
//!
//! ```json
#![doc = include_str!("../tests/fixtures/mailbox_snapshot.json")]
//! ```
//!
//! ```json
#![doc = include_str!("../tests/fixtures/mailbox_event.json")]
//! ```
//!
//! Client → server examples:
//!
//! ```json
#![doc = include_str!("../tests/fixtures/mailbox_update.json")]
//! ```
//!
//! ```json
#![doc = include_str!("../tests/fixtures/command.json")]
//! ```

pub mod convert;
pub mod dto;
pub mod error;
pub mod message;

pub use convert::{
    apply_records_to_bank, apply_snapshot_body_to_bank, records_from_update,
    snapshot_body_from_bank, snapshot_from_bank, system_status_from_dto, system_status_to_dto,
    zone_config_from_dto, zone_config_to_dto, zone_dto_from_state, zone_state_from_dto,
};
pub use dto::{AckStatus, SnapshotBody, SystemStatusDto, ZoneConfigDto, ZoneDto};
pub use error::EncodeError;
pub use message::{ClientMessage, ServerMessage};
